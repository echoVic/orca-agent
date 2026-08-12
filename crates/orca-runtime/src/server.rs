use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod command_exec_manager;
mod command_exec_sandbox;
mod connection_supervisor;
mod direct_interaction_adapter;
mod fuzzy_file_search_manager;
mod mention_search_manager;
mod opaque_permission_router;
mod router;
mod shell_manager;
mod surface_adapter;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};

use orca_core::approval_rules::{PermissionRule, PermissionRules};
use orca_core::approval_types::{ApprovalMode, Decision};
pub use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
use orca_mcp::McpRegistry;

use crate::network_proxy::{
    RuntimeNetworkBlockReport, RuntimeNetworkPolicy, RuntimeNetworkProxy,
    runtime_network_block_channel,
};
use crate::protocol::{self, ClientOp, ServerEvent, Submission};
use crate::runtime_event_projector::RuntimeEventProjector;
use crate::runtime_host::HostedOperationWriter;
use crate::sandbox_denial::{SandboxDenialDiagnostic, diagnose_sandbox_denial};
use crate::shell_session::{ShellSandboxMode, ShellSessionCommand};
use crate::thread_store::{
    SortDirection, StoredThreadItem, StoredThreadSummary, StoredThreadTurn, ThreadListFilters,
    ThreadMetadataPatch, ThreadSortKey, TurnItemsView,
};
use command_exec_manager::{
    CommandExecDrainOutcome, CommandExecManager, CommandExecPermissionPolicy, CommandExecProcess,
    CommandExecProcessSnapshot,
};
pub use command_exec_sandbox::{CommandExecSandbox, bash_sandbox_for_cwd};
use command_exec_sandbox::{command_exec_sandbox_mode, materialize_workspace_roots_paths};
use connection_supervisor::{
    JsonlConnectionServices, JsonlConnectionSupervisor, JsonlNonIoCloseTrigger,
    JsonlSupervisorCloseTrigger, JsonlSupervisorIoFailure,
};
use direct_interaction_adapter::JsonlDirectInteractionAdapter;
use fuzzy_file_search_manager::FuzzyFileSearchManager;
use mention_search_manager::MentionSearchManager;
use opaque_permission_router::{
    JsonlCommandExecPermissionRequest, JsonlCommittedReplay, JsonlConnectionAdmission,
    JsonlOpaquePermissionRouter, JsonlPermissionRoute, JsonlRetiredRequestOwner,
    JsonlRetiredRequestSettlement, jsonl_response_digest,
};
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use shell_manager::ServerShellManager;
pub use surface_adapter::JsonlSurfaceAdapter;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub run_config: RunConfig,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionProfileOverride {
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub approval_mode: Option<ApprovalMode>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: Option<PermissionRules>,
    pub permission_updates: Vec<PermissionUpdate>,
}

impl PermissionProfileOverride {
    pub fn is_empty(&self) -> bool {
        self.active_permission_profile.is_none()
            && self.approval_mode.is_none()
            && self.runtime_workspace_roots.is_none()
            && self.permission_rules.is_none()
            && self.permission_updates.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionUpdate {
    AddRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    ReplaceRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    RemoveRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    SetMode {
        destination: String,
        mode: ApprovalMode,
    },
    AddDirectories {
        directories: Vec<AdditionalWorkingDirectory>,
    },
    RemoveDirectories {
        destination: String,
        directories: Vec<PathBuf>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRuleValue {
    pub tool: String,
    pub pattern: Option<String>,
}

impl PermissionRuleValue {
    pub fn new(tool: impl Into<String>, pattern: Option<impl Into<String>>) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.map(Into::into),
        }
    }

    fn into_rule(self, behavior: Decision) -> PermissionRule {
        PermissionRule::new(
            self.tool,
            self.pattern.unwrap_or_else(|| "*".to_string()),
            behavior,
        )
    }

    fn matches_rule(&self, rule: &PermissionRule, behavior: Decision) -> bool {
        rule.decision == behavior
            && rule.tool == self.tool
            && self
                .pattern
                .as_deref()
                .map(|pattern| pattern == rule.pattern)
                .unwrap_or(true)
    }
}

pub(crate) struct ServerThreadSubmissionContext {
    pub(crate) cwd: String,
    pub(crate) runtime_workspace_roots: Vec<PathBuf>,
    pub(crate) mcp_registry: McpRegistry,
}

pub struct ServerThreadView {
    cwd: String,
    runtime_workspace_roots: Vec<PathBuf>,
    active_permission_profile: Option<ActivePermissionProfile>,
    additional_working_directories: Vec<AdditionalWorkingDirectory>,
    metadata_writable_directories: Vec<PathBuf>,
    network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    mcp_registry: McpRegistry,
}

impl ServerThreadView {
    pub fn additional_working_directories(&self) -> &[AdditionalWorkingDirectory] {
        &self.additional_working_directories
    }

    pub fn metadata_writable_directories(&self) -> &[PathBuf] {
        &self.metadata_writable_directories
    }

    pub fn active_permission_profile(&self) -> Option<&ActivePermissionProfile> {
        self.active_permission_profile.as_ref()
    }

    pub fn runtime_workspace_roots(&self) -> &[PathBuf] {
        &self.runtime_workspace_roots
    }

    pub fn network_domain_permissions(&self) -> &HashMap<String, PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn mcp_registry(&self) -> &McpRegistry {
        &self.mcp_registry
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerThreadTurn {
    prompt: String,
}

impl ServerThreadTurn {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

pub(crate) fn apply_permission_override(
    config: &mut RunConfig,
    permissions: PermissionProfileOverride,
) {
    if let Some(active_permission_profile) = permissions.active_permission_profile {
        config.active_permission_profile = Some(active_permission_profile);
    }
    if let Some(approval_mode) = permissions.approval_mode {
        config.approval_mode = approval_mode;
    }
    if let Some(runtime_workspace_roots) = permissions.runtime_workspace_roots {
        config.runtime_workspace_roots = Some(runtime_workspace_roots);
    }
    if let Some(permission_rules) = permissions.permission_rules {
        config.permission_rules = permission_rules;
    }
    apply_permission_updates(config, permissions.permission_updates);
}

fn apply_permission_updates(config: &mut RunConfig, updates: Vec<PermissionUpdate>) {
    for update in updates {
        match update {
            PermissionUpdate::SetMode { mode, .. } => config.approval_mode = mode,
            PermissionUpdate::AddRules {
                behavior, rules, ..
            } => config
                .permission_rules
                .rules
                .extend(rules.into_iter().map(|rule| rule.into_rule(behavior))),
            PermissionUpdate::ReplaceRules {
                behavior, rules, ..
            } => {
                config
                    .permission_rules
                    .rules
                    .retain(|rule| rule.decision != behavior);
                config
                    .permission_rules
                    .rules
                    .extend(rules.into_iter().map(|rule| rule.into_rule(behavior)));
            }
            PermissionUpdate::RemoveRules {
                behavior, rules, ..
            } => config.permission_rules.rules.retain(|rule| {
                !rules
                    .iter()
                    .any(|remove| remove.matches_rule(rule, behavior))
            }),
            PermissionUpdate::AddDirectories { directories } => {
                for directory in directories {
                    if let Some(existing) = config
                        .additional_working_directories
                        .iter_mut()
                        .find(|existing| existing.path == directory.path)
                    {
                        existing.source = directory.source;
                    } else {
                        config.additional_working_directories.push(directory);
                    }
                }
            }
            PermissionUpdate::RemoveDirectories {
                destination,
                directories,
            } => config.additional_working_directories.retain(|directory| {
                directory.source != destination
                    || !directories.iter().any(|remove| remove == &directory.path)
            }),
        }
    }
}

pub struct ServerRequestWriter<W: Write> {
    id: Value,
    inner: W,
    buffer: Vec<u8>,
    projector: RuntimeEventProjector,
}

impl<W: Write> ServerRequestWriter<W> {
    pub fn new(id: Value, inner: W) -> Self {
        Self {
            id,
            inner,
            buffer: Vec::new(),
            projector: RuntimeEventProjector::default(),
        }
    }

    pub fn flush_remaining(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            self.write_runtime_line(&line)?;
        }
        Ok(())
    }

    fn write_runtime_line(&mut self, line: &str) -> io::Result<()> {
        for event in self.projector.project_line(line) {
            protocol::write_server_event(&mut self.inner, &self.id, event)?;
        }
        Ok(())
    }

    fn write_server_event(&mut self, event: ServerEvent) -> io::Result<()> {
        protocol::write_server_event(&mut self.inner, &self.id, event)
    }
}

impl<W: Write> Write for ServerRequestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        while let Some(pos) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line = String::from_utf8_lossy(&self.buffer[..pos]).to_string();
            self.buffer.drain(..=pos);
            self.write_runtime_line(&line)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn thread_run_config(config: &RunConfig) -> RunConfig {
    let mut run_config = config.clone();
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = match run_config.history_mode {
        HistoryMode::Record => HistoryMode::Record,
        HistoryMode::Disabled
        | HistoryMode::Resume(_)
        | HistoryMode::ResumeAt { .. }
        | HistoryMode::Fork(_) => HistoryMode::Disabled,
    };
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;
    run_config
}

pub fn thread_turn_to_json(turn: StoredThreadTurn) -> Value {
    json!({
        "threadId": turn.thread_id,
        "turnId": turn.turn_id,
        "index": turn.index,
        "role": turn.role,
        "itemsView": turn_items_view_to_json(turn.items_view),
        "items": turn.items,
    })
}

pub fn thread_item_to_json(item: StoredThreadItem) -> Value {
    json!({
        "threadId": item.thread_id,
        "turnId": item.turn_id,
        "itemId": item.item_id,
        "index": item.index,
        "item": item.item,
    })
}

fn turn_items_view_to_json(items_view: TurnItemsView) -> &'static str {
    match items_view {
        TurnItemsView::NotLoaded => "notLoaded",
        TurnItemsView::Summary => "summary",
        TurnItemsView::Full => "full",
    }
}

pub fn run(config: ServerConfig) -> i32 {
    match run_with_io(config, io::stdin().lock(), io::stdout()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: server error: {error}");
            1
        }
    }
}

fn run_with_io<R: BufRead, W: Write + Send + 'static>(
    config: ServerConfig,
    mut reader: R,
    writer: W,
) -> io::Result<()> {
    let mut line = String::new();
    let mut state = ServerState::start()?;
    let writer = Arc::new(Mutex::new(writer));
    let mut close_trigger = JsonlSupervisorCloseTrigger::NonIo(JsonlNonIoCloseTrigger::EndOfFile);
    let result = (|| -> io::Result<()> {
        loop {
            let bytes_read = match reader.read_line(&mut line) {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    close_trigger = JsonlSupervisorCloseTrigger::Io(
                        JsonlSupervisorIoFailure::ReadFailed(error.to_string()),
                    );
                    return Err(error);
                }
            };
            if bytes_read == 0 {
                break;
            }
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if let Err(error) = handle_line(&config, &mut state, trimmed, Arc::clone(&writer)) {
                    close_trigger = JsonlSupervisorCloseTrigger::Io(
                        JsonlSupervisorIoFailure::WriteFailed(error.to_string()),
                    );
                    return Err(error);
                }
            }
            line.clear();
        }
        // The connection supervisor owns clean-EOF one-shot completion after it
        // settles any now-unreachable interaction routes.
        Ok(())
    })();
    let shutdown = state.shutdown(close_trigger);
    result.and(shutdown)
}

struct ServerState {
    supervisor: JsonlConnectionSupervisor,
    threads: JsonlSurfaceAdapter,
    shells: ServerShellManager,
    command_exec: CommandExecManager,
    permission_routes: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
    direct_interactions:
        JsonlDirectInteractionAdapter<direct_interaction_adapter::JsonlDirectInteractionRoute>,
    fuzzy_file_searches: FuzzyFileSearchManager,
    mention_searches: MentionSearchManager,
}

impl ServerState {
    fn start() -> io::Result<Self> {
        let threads = JsonlSurfaceAdapter::start()?;
        let connection_id = threads
            .connection_id()
            .ok_or_else(|| io::Error::other("JSONL surface connection is not bound"))?;
        let admission = JsonlConnectionAdmission::new(connection_id);
        let permission_router = JsonlOpaquePermissionRouter::new(admission.clone());
        let direct_interactions = JsonlDirectInteractionAdapter::new(admission.clone());
        let supervisor = JsonlConnectionSupervisor::new(
            admission,
            permission_router.clone(),
            direct_interactions.clone(),
        );
        Ok(Self {
            supervisor,
            threads,
            shells: ServerShellManager::default(),
            command_exec: CommandExecManager::default(),
            permission_routes: permission_router,
            direct_interactions,
            fuzzy_file_searches: FuzzyFileSearchManager::default(),
            mention_searches: MentionSearchManager::default(),
        })
    }

    fn shutdown(self, trigger: JsonlSupervisorCloseTrigger) -> io::Result<()> {
        let Self {
            supervisor,
            threads,
            shells,
            command_exec,
            permission_routes: _,
            direct_interactions: _,
            fuzzy_file_searches,
            mention_searches,
        } = self;
        supervisor
            .close(
                trigger,
                JsonlConnectionServices {
                    threads,
                    shells,
                    command_exec,
                    fuzzy_file_searches,
                    mention_searches,
                },
            )
            .into_io_result()
    }
}

impl ServerState {
    #[cfg(test)]
    fn join_active_turns(&mut self) {
        self.threads.wait_active_turns();
    }

    fn prune_finished_turns(&mut self) {
        self.threads.prune_finished_turns();
    }
}

#[cfg(test)]
impl Default for ServerState {
    fn default() -> Self {
        Self::start().expect("start server state")
    }
}

fn handle_line<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    line: &str,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    state.prune_finished_turns();
    let submission = match Submission::decode(line) {
        Ok(submission) => submission,
        Err(error) => {
            write_locked_event(&writer, &error.id, ServerEvent::error(error.message))?;
            return Ok(());
        }
    };
    if let ClientOp::CommandExecRead {
        process_id,
        output_bytes_cap: Some(output_bytes_cap),
        ..
    } = &submission.op
    {
        state
            .command_exec
            .tighten_output_cap(process_id, *output_bytes_cap);
    }
    {
        let mut writer = writer.lock().map_err(lock_error)?;
        match drain_command_exec_processes(state, &mut *writer)? {
            CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                request_command_exec_network_permission(state, request, block, &mut *writer)?;
            }
            CommandExecDrainOutcome::NetworkPermissionDenied {
                command_event_id,
                reason,
            } => {
                protocol::write_server_event(
                    &mut *writer,
                    &command_event_id,
                    ServerEvent::error(reason),
                )?;
            }
            CommandExecDrainOutcome::FileSystemPermissionRequired {
                request,
                diagnostic,
            } => {
                request_command_exec_file_system_permission(
                    state,
                    request,
                    diagnostic,
                    &mut *writer,
                )?;
            }
            CommandExecDrainOutcome::Drained => {}
        }
    }

    router::dispatch_submission(config, state, submission, writer)?;
    state.prune_finished_turns();
    Ok(())
}

#[cfg(test)]
fn handle_line_for_test(
    config: &ServerConfig,
    state: &mut ServerState,
    line: &str,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(Vec::new()));
    handle_line(config, state, line, Arc::clone(&writer))?;
    state.join_active_turns();
    let mut writer = writer.lock().map_err(lock_error)?;
    output.extend_from_slice(&writer);
    writer.clear();
    Ok(())
}

fn write_locked_event<W: Write>(
    writer: &Arc<Mutex<W>>,
    id: &Value,
    event: ServerEvent,
) -> io::Result<()> {
    let mut writer = writer.lock().map_err(lock_error)?;
    protocol::write_server_event(&mut *writer, id, event)
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("server writer lock poisoned")
}

struct SharedServerRequestWriter<W: Write> {
    inner: Arc<Mutex<W>>,
    writer: ServerRequestWriter<LockedServerWriter<W>>,
}

impl<W: Write> SharedServerRequestWriter<W> {
    fn new(id: Value, inner: Arc<Mutex<W>>) -> Self {
        let locked = LockedServerWriter {
            inner: Arc::clone(&inner),
        };
        Self {
            inner,
            writer: ServerRequestWriter::new(id, locked),
        }
    }

    fn flush_remaining(&mut self) -> io::Result<()> {
        self.writer.flush_remaining()
    }

    fn write_server_event(&mut self, event: ServerEvent) -> io::Result<()> {
        self.writer.write_server_event(event)
    }
}

impl<W: Write> Write for SharedServerRequestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().map_err(lock_error)?.flush()
    }
}

struct ServerTurnOutput<W: Write + Send + 'static> {
    inner: SharedServerRequestWriter<W>,
    buffer: Vec<u8>,
    deferred_lines: Vec<Vec<u8>>,
}

impl<W: Write + Send + 'static> ServerTurnOutput<W> {
    fn new(id: Value, inner: Arc<Mutex<W>>) -> Self {
        Self {
            inner: SharedServerRequestWriter::new(id, inner),
            buffer: Vec::new(),
            deferred_lines: Vec::new(),
        }
    }

    fn process_line(&mut self, line: Vec<u8>) -> io::Result<()> {
        if is_runtime_generation_terminal_line(&line) {
            self.deferred_lines.push(line);
        } else {
            self.inner.write_all(&line)?;
        }
        Ok(())
    }

    fn flush_remaining(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.process_line(line)?;
        }
        self.inner.flush_remaining()
    }

    fn finish(&mut self, commit_terminal: bool) -> io::Result<()> {
        self.flush_remaining()?;
        if commit_terminal {
            for line in self.deferred_lines.drain(..) {
                self.inner.write_all(&line)?;
            }
            self.inner.flush_remaining()?;
        } else {
            self.deferred_lines.clear();
        }
        Ok(())
    }
}

impl<W: Write + Send + 'static> Write for ServerTurnOutput<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        while let Some(pos) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=pos).collect::<Vec<_>>();
            self.process_line(line)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write + Send + 'static> HostedOperationWriter for ServerTurnOutput<W> {
    fn finish_generation(&mut self, commit_terminal: bool) -> io::Result<()> {
        self.finish(commit_terminal)
    }
}

impl<W: Write + Send + 'static> surface_adapter::JsonlSurfaceOutput for ServerTurnOutput<W> {
    fn write_server_event(&mut self, event: ServerEvent) -> io::Result<()> {
        self.inner.write_server_event(event)
    }

    fn supports_direct_server_events(&self) -> bool {
        true
    }
}

#[derive(serde::Deserialize)]
struct RuntimeGenerationEventLine {
    #[serde(rename = "type")]
    event_type: orca_core::event_schema::EventType,
    payload: Value,
}

fn is_runtime_generation_terminal_line(line: &[u8]) -> bool {
    let Ok(event) = serde_json::from_slice::<RuntimeGenerationEventLine>(line) else {
        return false;
    };
    match event.event_type {
        orca_core::event_schema::EventType::SessionCompleted => true,
        orca_core::event_schema::EventType::Error => {
            event.payload["message"].as_str() == Some("turn cancelled")
        }
        _ => false,
    }
}

struct LockedServerWriter<W: Write> {
    inner: Arc<Mutex<W>>,
}

impl<W: Write> Write for LockedServerWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.lock().map_err(lock_error)?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().map_err(lock_error)?.flush()
    }
}

struct PersistedSessionPermissionGrant {
    additional_working_directories: Vec<orca_core::config::AdditionalWorkingDirectory>,
    metadata_writable_directories: Vec<PathBuf>,
    network_domain_permissions: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn materialize_session_permission_grant(
    threads: &JsonlSurfaceAdapter,
    thread_id: &str,
    runtime_workspace_roots: &[PathBuf],
    permissions: &protocol::RequestPermissionProfile,
) -> io::Result<PersistedSessionPermissionGrant> {
    let file_system = permissions.file_system.as_ref();
    let roots = file_system
        .into_iter()
        .flat_map(|file_system| file_system.write.iter().flatten())
        .filter(|path| !path.as_os_str().is_empty());
    let mut thread = threads.read_thread_result(thread_id, false, false)?;
    for root in roots {
        for root in materialize_workspace_roots_paths(&thread.cwd, runtime_workspace_roots, root) {
            if orca_tools::sandbox::is_protected_metadata_root(&root) {
                push_unique_path(&mut thread.metadata_writable_directories, root);
            } else if !thread
                .additional_working_directories
                .iter()
                .any(|directory| directory.path == root)
            {
                thread.additional_working_directories.push(
                    orca_core::config::AdditionalWorkingDirectory::new(root, "session"),
                );
            }
        }
    }
    if let Some(network) = permissions.network.as_ref() {
        for (domain, access) in &network.domains {
            thread
                .network_domain_permissions
                .insert(domain.clone(), *access);
        }
    }
    Ok(PersistedSessionPermissionGrant {
        additional_working_directories: thread.additional_working_directories,
        metadata_writable_directories: thread.metadata_writable_directories,
        network_domain_permissions: thread.network_domain_permissions,
    })
}

fn run_shell_start<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: Option<&str>,
    command: &str,
    description: Option<String>,
    terminal: crate::shell_session::ShellTerminalMode,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell command must not be empty"),
        );
    }
    let cwd = server_cwd(&config.run_config)?;
    let task_registry = match thread_id {
        Some(thread_id) => match state.threads.task_registry(thread_id) {
            Some(registry) => Some(registry),
            None => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("unknown thread: {thread_id}")),
                );
            }
        },
        None => None,
    };
    let command_text = command.to_string();
    let command = ShellSessionCommand {
        command: command_text.clone(),
        argv: None,
        cwd: cwd.clone(),
        additional_readable_directories: Vec::new(),
        additional_working_directories: Vec::new(),
        denied_working_directories: Vec::new(),
        allowed_unix_socket_roots: Vec::new(),
        env: Default::default(),
        description: description.unwrap_or_else(|| command_text.clone()),
        terminal,
        sandbox: ShellSandboxMode::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
    };
    let handle = match state.shells.spawn(&cwd, command, task_registry) {
        Ok(handle) => handle,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("failed to start shell: {error}")),
            );
        }
    };
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellStarted {
            shell_id: Value::from(handle.id),
            task_id: Value::from(handle.task_id),
            command: Value::from(command_text),
            status: Value::from("running"),
            requested_terminal_mode: Value::from(handle.requested_terminal.as_str()),
            effective_terminal_mode: Value::from(handle.effective_terminal.as_str()),
        },
    )
}

fn run_shell_capabilities<W: Write>(id: Value, writer: &mut W) -> io::Result<()> {
    let capabilities = crate::shell_session::shell_runtime_capabilities();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellCapabilities {
            platform: Value::from(capabilities.platform),
            supports_pty: Value::from(capabilities.supports_pty),
            supports_pty_resize: Value::from(capabilities.supports_pty_resize),
            supported_terminal_modes: Value::from(vec![Value::from("pipe"), Value::from("pty")]),
            fallback_terminal_mode: Value::from(capabilities.fallback_terminal_mode.as_str()),
            command_exec_streaming_requires_process_id: Value::from(
                capabilities.command_exec_streaming_requires_process_id,
            ),
        },
    )
}

fn run_shell_write<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    input: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.write_stdin(shell_id, input) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("running"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_update<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    description: Option<&str>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(description) = description else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell update did not include any supported fields"),
        );
    };
    let Some(result) = state.shells.update_description(shell_id, description) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("updated"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::from(description.trim().to_string()),
        },
    )
}

fn run_shell_close<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.close_stdin(shell_id) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("stdin_closed"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_resize<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    cols: u16,
    rows: u16,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if cols == 0 || rows == 0 {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell resize cols and rows must be greater than zero"),
        );
    }
    let Some(result) = state.shells.resize(shell_id, cols, rows) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("resized"),
            cols: Value::from(cols),
            rows: Value::from(rows),
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_list<W: Write>(state: &mut ServerState, id: Value, writer: &mut W) -> io::Result<()> {
    let command_exec_shell_ids = state.command_exec.active_shell_ids();
    for output in state.shells.reap_requested_stops()? {
        write_shell_completed(writer, &id, output)?;
    }
    for output in state
        .shells
        .reap_completed_except(&command_exec_shell_ids)?
    {
        write_shell_completed(writer, &id, output)?;
    }
    let shells = state
        .shells
        .list()
        .into_iter()
        .filter(|snapshot| !command_exec_shell_ids.contains(&snapshot.id))
        .map(shell_snapshot_to_json)
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellListed {
            shells: Value::from(shells),
        },
    )
}

fn run_shell_read<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    timeout_ms: u64,
    output_bytes_cap: Option<usize>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    for output in state.shells.reap_requested_stops()? {
        if output.id == shell_id {
            return write_shell_completed_with_cap(writer, &id, output, output_bytes_cap);
        }
        write_shell_completed(writer, &id, output)?;
    }
    let Some(result) = state
        .shells
        .read(shell_id, Duration::from_millis(timeout_ms.max(1)))
    else {
        return unknown_shell(writer, &id, shell_id);
    };
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    if output.status == orca_core::task_types::TaskStatus::Running {
        let stdout = cap_text(&output.stdout, output_bytes_cap);
        let stderr = cap_text(&output.stderr, output_bytes_cap);
        let cap_reached = shell_output_cap_reached(&output, output_bytes_cap);
        write_shell_output_deltas_with_cap(writer, &id, &output, false, output_bytes_cap)?;
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ShellUpdated {
                shell_id: Value::from(output.id),
                status: Value::from("running"),
                cols: Value::Null,
                rows: Value::Null,
                stdout: Value::from(stdout),
                stderr: Value::from(stderr),
                exit_code: Value::Null,
                cap_reached: shell_cap_reached_value(output_bytes_cap, cap_reached),
                description: Value::Null,
            },
        )
    } else {
        write_shell_completed_with_cap(writer, &id, output, output_bytes_cap)
    }
}

fn run_shell_kill<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.kill(shell_id) else {
        return unknown_shell(writer, &id, shell_id);
    };
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    write_shell_completed(writer, &id, output)
}

fn run_command_exec<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: Option<&str>,
    command: &[String],
    command_is_argv: bool,
    process_id: Option<&str>,
    cwd: Option<&PathBuf>,
    env: &protocol::CommandEnvOverrides,
    options: &protocol::CommandExecOptions,
    approved_permissions: Option<&protocol::RequestPermissionProfile>,
    terminal: crate::shell_session::ShellTerminalMode,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if command.is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec command must not be empty"),
        );
    }
    if options.sandbox_policy != protocol::CommandSandboxPolicy::Default
        && options.permission_profile.is_some()
    {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("`permissionProfile` cannot be combined with `sandboxPolicy`"),
        );
    }
    if options.disable_timeout && options.timeout_ms.is_some() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec cannot set both timeoutMs and disableTimeout"),
        );
    }
    if options.disable_output_cap && options.output_bytes_cap.is_some() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec cannot set both outputBytesCap and disableOutputCap"),
        );
    }
    if options.has_size && !terminal.is_pty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size requires tty: true"),
        );
    }
    let (terminal_cols, terminal_rows) = terminal.size();
    if terminal_cols == Some(0) || terminal_rows == Some(0) {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size rows and cols must be greater than 0"),
        );
    }
    let timeout_ms = match options.timeout_ms {
        Some(timeout_ms) => match u64::try_from(timeout_ms) {
            Ok(timeout_ms) => timeout_ms,
            Err(_) => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "command/exec timeoutMs must be non-negative, got {timeout_ms}"
                    )),
                );
            }
        },
        None => 120_000,
    };
    if process_id.is_none()
        && (terminal.is_pty() || options.stream_stdin || options.stream_stdout_stderr)
    {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(
                "command/exec tty or streaming requires a client-supplied processId",
            ),
        );
    }
    let command_text = if command_is_argv {
        protocol::shell_join(command)
    } else {
        command[0].clone()
    };
    let cwd = cwd.cloned().unwrap_or(server_cwd(&config.run_config)?);
    let (
        mut additional_working_directories,
        mut metadata_writable_directories,
        thread_permission_profile,
        runtime_workspace_roots,
        thread_network_domain_permissions,
    ) = match thread_id {
        Some(thread_id) => {
            state.prune_finished_turns();
            match state.threads.thread(thread_id) {
                Some(thread) => (
                    thread
                        .additional_working_directories()
                        .iter()
                        .map(|directory| directory.path.clone())
                        .collect(),
                    thread.metadata_writable_directories().to_vec(),
                    thread.active_permission_profile().cloned(),
                    thread.runtime_workspace_roots().to_vec(),
                    thread.network_domain_permissions().clone(),
                ),
                None => {
                    return protocol::write_server_event(
                        writer,
                        &id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    );
                }
            }
        }
        None => (
            Vec::new(),
            Vec::new(),
            None,
            config
                .run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_default(),
            HashMap::new(),
        ),
    };
    let mut effective_sandbox = match command_exec_sandbox_mode(
        &config.run_config,
        options,
        thread_permission_profile.as_ref(),
        &cwd,
        &runtime_workspace_roots,
        std::env::var_os("TMPDIR").map(PathBuf::from).as_deref(),
    ) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            return protocol::write_server_event(writer, &id, ServerEvent::error(error));
        }
    };
    for (domain, access) in thread_network_domain_permissions {
        match access {
            orca_core::config::PermissionProfileNetworkAccess::Deny => {
                effective_sandbox
                    .network_policy_domains
                    .insert(domain, access);
            }
            orca_core::config::PermissionProfileNetworkAccess::Allow => {
                effective_sandbox
                    .network_policy_domains
                    .entry(domain)
                    .or_insert(access);
            }
        }
    }
    additional_working_directories.extend(effective_sandbox.additional_writable_roots.clone());
    metadata_writable_directories.extend(effective_sandbox.metadata_writable_roots.clone());
    if let Some(file_system) =
        approved_permissions.and_then(|permissions| permissions.file_system.as_ref())
    {
        for requested in file_system.write.iter().flatten() {
            for root in materialize_workspace_roots_paths(
                &cwd.display().to_string(),
                &runtime_workspace_roots,
                requested,
            ) {
                if orca_tools::sandbox::is_protected_metadata_root(&root) {
                    push_unique_path(&mut metadata_writable_directories, root);
                } else {
                    push_unique_path(&mut additional_working_directories, root);
                }
            }
        }
    }
    let denied_writable_directories = effective_sandbox.denied_writable_roots.clone();
    if let protocol::CommandSandboxPolicy::WorkspaceWrite { writable_roots, .. } =
        &options.sandbox_policy
    {
        additional_working_directories.extend(writable_roots.iter().cloned());
    }
    #[cfg(windows)]
    if !effective_sandbox.network_policy_domains.is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(
                "Windows domain-restricted network sandbox is unavailable; refusing to run without an OS-enforced network boundary",
            ),
        );
    }
    let mut retry_block_reporter = None;
    let mut retry_block_receiver = None;
    let command_permission_request = thread_id.map(|thread_id| JsonlCommandExecPermissionRequest {
        thread_id: thread_id.to_string(),
        runtime_workspace_roots: runtime_workspace_roots.clone(),
        command: command.to_vec(),
        command_is_argv,
        process_id: process_id.map(ToString::to_string),
        cwd: Some(cwd.clone()),
        env: env.clone(),
        options: options.clone(),
        terminal,
        event_id: id.clone(),
    });
    if !effective_sandbox.network_policy_domains.is_empty() {
        let (block_sender, block_receiver) = runtime_network_block_channel();
        retry_block_reporter = Some(block_sender);
        retry_block_receiver = Some(block_receiver);
    }
    if let Some(process_id) = process_id {
        if let Err(error) = state.command_exec.insert(
            process_id.to_string(),
            CommandExecProcess {
                shell_id: None,
                command_event_id: id.clone(),
                command: command.to_vec(),
                cwd: cwd.clone(),
                denied_writable_roots: denied_writable_directories.clone(),
                stream_output: terminal.is_pty() || options.stream_stdout_stderr,
                output_bytes_cap: options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
                output_offset: 0,
                stdout_len: 0,
                stderr_len: 0,
                stdout_cap_reached: false,
                stderr_cap_reached: false,
                network_permission_blocks: retry_block_receiver.take(),
                permission_request: command_permission_request.clone(),
                _network_proxy: None,
            },
        ) {
            return protocol::write_server_event(writer, &id, ServerEvent::error(error));
        }
    }
    let mut network_proxy = if effective_sandbox.network_policy_domains.is_empty() {
        None
    } else {
        match RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(effective_sandbox.network_policy_domains.clone()),
            retry_block_reporter,
        ) {
            Ok(proxy) => Some(proxy),
            Err(error) => {
                if let Some(process_id) = process_id {
                    state.command_exec.remove(process_id);
                }
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("failed to start network proxy: {error}")),
                );
            }
        }
    };
    let mut command_env = env.clone();
    if let Some(proxy) = network_proxy.as_ref() {
        let proxy_url = proxy.proxy_url().to_string();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command_env.insert(key.to_string(), Some(proxy_url.clone()));
        }
        for key in ["NO_PROXY", "no_proxy"] {
            command_env.insert(key.to_string(), None);
        }
    }
    let handle = match state.shells.spawn_with_metadata_roots(
        &cwd,
        ShellSessionCommand {
            command: command_text.clone(),
            argv: command_is_argv.then(|| command.to_vec()),
            cwd: cwd.clone(),
            additional_readable_directories: effective_sandbox.additional_readable_roots,
            additional_working_directories,
            denied_working_directories: denied_writable_directories.clone(),
            allowed_unix_socket_roots: effective_sandbox.allowed_unix_socket_roots,
            env: command_env,
            description: command_text,
            terminal,
            sandbox: effective_sandbox.mode,
        },
        metadata_writable_directories,
        None,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(process_id) = process_id {
                state.command_exec.remove(process_id);
            }
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("failed to start command: {error}")),
            );
        }
    };
    if let Some(process_id) = process_id {
        if let Some(proxy) = network_proxy.take() {
            state.command_exec.retain_network_proxy(process_id, proxy);
        }
        state.command_exec.activate(process_id, handle.id);
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::CommandExecStarted {
                process_id: Value::from(process_id.to_string()),
            },
        )?;
        let drain_outcome = if terminal.is_pty() || options.stream_stdout_stderr {
            drain_command_exec_processes_until_output_or_timeout(
                state,
                writer,
                Duration::from_secs(1),
            )?
        } else {
            drain_command_exec_processes_with_timeout(state, writer, Duration::from_millis(250))?
        };
        match drain_outcome {
            CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                return request_command_exec_network_permission(state, request, block, writer);
            }
            CommandExecDrainOutcome::NetworkPermissionDenied {
                command_event_id,
                reason,
            } => {
                return protocol::write_server_event(
                    writer,
                    &command_event_id,
                    ServerEvent::error(reason),
                );
            }
            CommandExecDrainOutcome::FileSystemPermissionRequired {
                request,
                diagnostic,
            } => {
                return request_command_exec_file_system_permission(
                    state, request, diagnostic, writer,
                );
            }
            CommandExecDrainOutcome::Drained => {}
        }
        return Ok(());
    }

    let mut output = match state
        .shells
        .wait(&handle.id, Duration::from_millis(timeout_ms.max(1)))
    {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    if let Some(blocked_hosts) = retry_block_receiver
        && let Some(block) = CommandExecPermissionPolicy::network_permission_block(blocked_hosts)
    {
        if let Some(denial) = CommandExecPermissionPolicy::network_block_denial(&block) {
            return protocol::write_server_event(writer, &id, ServerEvent::error(denial.reason));
        }
        if let Some(request) = command_permission_request.clone() {
            return request_command_exec_network_permission(state, request, block, writer);
        }
    }
    if let Some(diagnostic) = diagnose_sandbox_denial(&cwd, &output.stdout, &output.stderr) {
        if CommandExecPermissionPolicy::should_request_filesystem_retry(
            &cwd,
            &diagnostic,
            &denied_writable_directories,
        ) && let Some(request) = command_permission_request
        {
            return request_command_exec_file_system_permission(state, request, diagnostic, writer);
        }
        append_sandbox_diagnostic_to_stderr(&mut output.stderr, &diagnostic);
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::CommandExecCompleted {
            process_id: Value::Null,
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
            stdout: Value::from(cap_text(
                &output.stdout,
                options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
            )),
            stderr: Value::from(cap_text(
                &output.stderr,
                options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
            )),
        },
    )
}

fn request_command_exec_network_permission<W: Write>(
    state: &mut ServerState,
    request: JsonlCommandExecPermissionRequest,
    block: RuntimeNetworkBlockReport,
    writer: &mut W,
) -> io::Result<()> {
    let prompt = CommandExecPermissionPolicy::network_block_prompt(&block)
        .expect("command/exec network permission prompts require requestable blocks");
    let (_origin, _kind, reason, permissions) = prompt.into_request_parts();
    request_command_exec_permission(state, request, reason, permissions, writer)
}

fn request_command_exec_file_system_permission<W: Write>(
    state: &mut ServerState,
    request: JsonlCommandExecPermissionRequest,
    diagnostic: SandboxDenialDiagnostic,
    writer: &mut W,
) -> io::Result<()> {
    let prompt = CommandExecPermissionPolicy::sandbox_denial_prompt(&diagnostic);
    let (_origin, _kind, reason, permissions) = prompt.into_request_parts();
    request_command_exec_permission(state, request, reason, permissions, writer)
}

fn request_command_exec_permission<W: Write>(
    state: &mut ServerState,
    request: JsonlCommandExecPermissionRequest,
    reason: String,
    permissions: protocol::RequestPermissionProfile,
    writer: &mut W,
) -> io::Result<()> {
    let thread_id = request.thread_id.clone();
    let request_id = format!(
        "permission-command-{}",
        request
            .event_id
            .as_str()
            .map(ToString::to_string)
            .unwrap_or_else(|| request.event_id.to_string())
    );
    let request_id = state.permission_routes.register(
        request_id,
        JsonlRetiredRequestOwner::CommandExecPermission,
        JsonlPermissionRoute::CommandExec {
            request: Box::new(request),
        },
    )?;
    let frame_digest = jsonl_response_digest(&json!({
        "id": &request_id,
        "event": "permission_request",
        "requestId": &request_id,
        "threadId": &thread_id,
        "turnId": Value::Null,
        "reason": &reason,
        "permissions": &permissions,
    }))?;
    state
        .permission_routes
        .mark_writing(&request_id, frame_digest)?;
    protocol::write_server_event(
        writer,
        &Value::from(request_id.clone()),
        ServerEvent::PermissionRequest {
            request_id: json!(request_id.clone()),
            thread_id: json!(thread_id),
            turn_id: Value::Null,
            reason: json!(reason),
            permissions: serde_json::to_value(&permissions).unwrap_or(Value::Null),
        },
    )?;
    writer.flush()?;
    state
        .permission_routes
        .mark_published(&request_id, frame_digest)
}

fn append_sandbox_diagnostic_to_stderr(stderr: &mut String, diagnostic: &SandboxDenialDiagnostic) {
    if stderr.trim_end().is_empty() {
        *stderr = diagnostic.message.clone();
    } else {
        stderr.push_str(&format!("\n\nSandbox diagnostic: {}", diagnostic.message));
    }
}

fn run_command_exec_list<W: Write>(
    state: &mut ServerState,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let shell_snapshots = state
        .shells
        .list()
        .into_iter()
        .map(|snapshot| (snapshot.id.clone(), snapshot))
        .collect::<HashMap<_, _>>();
    let processes = state
        .command_exec
        .list()
        .into_iter()
        .map(|snapshot| {
            let shell_snapshot = snapshot
                .shell_id
                .as_ref()
                .and_then(|shell_id| shell_snapshots.get(shell_id));
            command_exec_snapshot_to_json(snapshot, shell_snapshot)
        })
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::CommandExecListed {
            processes: Value::from(processes),
        },
    )
}

fn run_command_exec_write<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    delta_base64: Option<&str>,
    close_stdin: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if delta_base64.is_none() && !close_stdin {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec/write requires deltaBase64 or closeStdin"),
        );
    }
    state.command_exec.write_to_process(
        state.shells.sessions_mut(),
        process_id,
        delta_base64,
        close_stdin,
        &id,
        writer,
    )
}

fn run_command_exec_read<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    timeout_ms: u64,
    output_bytes_cap: Option<usize>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let outcome = state.command_exec.read_process(
        state.shells.sessions_mut(),
        process_id,
        Duration::from_millis(timeout_ms.max(1)),
        output_bytes_cap,
        &id,
        writer,
    )?;
    match outcome {
        CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
            request_command_exec_network_permission(state, request, block, writer)
        }
        CommandExecDrainOutcome::NetworkPermissionDenied {
            command_event_id,
            reason,
        } => protocol::write_server_event(writer, &command_event_id, ServerEvent::error(reason)),
        CommandExecDrainOutcome::FileSystemPermissionRequired {
            request,
            diagnostic,
        } => request_command_exec_file_system_permission(state, request, diagnostic, writer),
        CommandExecDrainOutcome::Drained => Ok(()),
    }
}

fn run_command_exec_resize<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    cols: u16,
    rows: u16,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if cols == 0 || rows == 0 {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size rows and cols must be greater than 0"),
        );
    }
    state.command_exec.resize_process(
        state.shells.sessions_mut(),
        process_id,
        cols,
        rows,
        &id,
        writer,
    )
}

fn drain_command_exec_processes<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain(state.shells.sessions_mut(), writer)
}

fn drain_command_exec_processes_with_timeout<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain_with_timeout(state.shells.sessions_mut(), writer, timeout)
}

fn drain_command_exec_processes_until_output_or_timeout<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain_until_output_or_timeout(state.shells.sessions_mut(), writer, timeout)
}

fn cap_text(text: &str, cap: Option<usize>) -> String {
    let Some(cap) = cap else {
        return text.to_string();
    };
    let visible_len = capped_utf8_len(text, cap);
    text[..visible_len].to_string()
}

fn capped_utf8_len(text: &str, cap: usize) -> usize {
    if cap >= text.len() {
        return text.len();
    }
    let mut len = cap;
    while len > 0 && !text.is_char_boundary(len) {
        len -= 1;
    }
    len
}

fn write_command_exec_output_deltas<W: Write>(
    writer: &mut W,
    process_id: &str,
    stdout_delta: &str,
    stderr_delta: &str,
    stdout_cap_reached: bool,
    stderr_cap_reached: bool,
    final_chunk: bool,
) -> io::Result<()> {
    if !stdout_delta.is_empty() {
        protocol::write_server_event(
            writer,
            &Value::Null,
            ServerEvent::CommandExecOutputDelta {
                process_id: Value::from(process_id.to_string()),
                stream: Value::from("stdout"),
                delta: Value::from(stdout_delta.to_string()),
                delta_base64: Value::from(BASE64_STANDARD.encode(stdout_delta.as_bytes())),
                cap_reached: Value::from(stdout_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    if !stderr_delta.is_empty() {
        protocol::write_server_event(
            writer,
            &Value::Null,
            ServerEvent::CommandExecOutputDelta {
                process_id: Value::from(process_id.to_string()),
                stream: Value::from("stderr"),
                delta: Value::from(stderr_delta.to_string()),
                delta_base64: Value::from(BASE64_STANDARD.encode(stderr_delta.as_bytes())),
                cap_reached: Value::from(stderr_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    Ok(())
}

fn run_command_exec_terminate<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    state
        .command_exec
        .terminate_process(state.shells.sessions_mut(), process_id, &id, writer)
}

fn write_shell_completed<W: Write>(
    writer: &mut W,
    id: &Value,
    output: crate::shell_session::ShellSessionOutput,
) -> io::Result<()> {
    write_shell_completed_with_cap(writer, id, output, None)
}

fn write_shell_completed_with_cap<W: Write>(
    writer: &mut W,
    id: &Value,
    output: crate::shell_session::ShellSessionOutput,
    output_bytes_cap: Option<usize>,
) -> io::Result<()> {
    let stdout = cap_text(&output.stdout, output_bytes_cap);
    let stderr = cap_text(&output.stderr, output_bytes_cap);
    let cap_reached = shell_output_cap_reached(&output, output_bytes_cap);
    write_shell_output_deltas_with_cap(writer, id, &output, true, output_bytes_cap)?;
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::ShellExited {
            shell_id: Value::from(output.id.clone()),
            task_id: Value::from(output.task_id.clone()),
            status: Value::from(shell_status_label(output.status)),
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
        },
    )?;
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::ShellCompleted {
            shell_id: Value::from(output.id),
            task_id: Value::from(output.task_id),
            status: Value::from(shell_status_label(output.status)),
            stdout: Value::from(stdout),
            stderr: Value::from(stderr),
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
            cap_reached: shell_cap_reached_value(output_bytes_cap, cap_reached),
        },
    )
}

fn write_shell_output_deltas_with_cap<W: Write>(
    writer: &mut W,
    id: &Value,
    output: &crate::shell_session::ShellSessionOutput,
    final_chunk: bool,
    output_bytes_cap: Option<usize>,
) -> io::Result<()> {
    let stdout = cap_text(&output.stdout, output_bytes_cap);
    let stderr = cap_text(&output.stderr, output_bytes_cap);
    let stdout_cap_reached =
        output_bytes_cap.is_some_and(|cap| output.stdout.len() >= cap && !output.stdout.is_empty());
    let stderr_cap_reached =
        output_bytes_cap.is_some_and(|cap| output.stderr.len() >= cap && !output.stderr.is_empty());
    if !stdout.is_empty() {
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::ShellOutputDelta {
                shell_id: Value::from(output.id.clone()),
                stream: Value::from("stdout"),
                delta: Value::from(stdout),
                cap_reached: Value::from(stdout_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    if !stderr.is_empty() {
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::ShellOutputDelta {
                shell_id: Value::from(output.id.clone()),
                stream: Value::from("stderr"),
                delta: Value::from(stderr),
                cap_reached: Value::from(stderr_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    Ok(())
}

fn shell_output_cap_reached(
    output: &crate::shell_session::ShellSessionOutput,
    output_bytes_cap: Option<usize>,
) -> bool {
    output_bytes_cap.is_some_and(|cap| output.stdout.len() >= cap || output.stderr.len() >= cap)
}

fn shell_cap_reached_value(output_bytes_cap: Option<usize>, cap_reached: bool) -> Value {
    if output_bytes_cap.is_some() {
        Value::from(cap_reached)
    } else {
        Value::Null
    }
}

fn shell_snapshot_to_json(snapshot: crate::shell_session::ShellSessionSnapshot) -> Value {
    json!({
        "shellId": snapshot.id,
        "taskId": snapshot.task_id,
        "command": snapshot.command,
        "description": snapshot.description,
        "status": shell_status_label(snapshot.status),
        "requestedTerminalMode": snapshot.requested_terminal.as_str(),
        "effectiveTerminalMode": snapshot.effective_terminal.as_str(),
    })
}

fn command_exec_snapshot_to_json(
    snapshot: CommandExecProcessSnapshot,
    shell_snapshot: Option<&crate::shell_session::ShellSessionSnapshot>,
) -> Value {
    json!({
        "processId": snapshot.process_id,
        "shellId": snapshot.shell_id,
        "taskId": shell_snapshot.map(|shell| shell.task_id.clone()),
        "command": snapshot.command,
        "cwd": snapshot.cwd.display().to_string(),
        "status": shell_snapshot
            .map(|shell| shell_status_label(shell.status))
            .unwrap_or(snapshot.status),
        "requestedTerminalMode": shell_snapshot.map(|shell| shell.requested_terminal.as_str()),
        "effectiveTerminalMode": shell_snapshot.map(|shell| shell.effective_terminal.as_str()),
        "streamOutput": snapshot.stream_output,
        "outputBytesCap": snapshot.output_bytes_cap,
        "stdoutBytes": snapshot.stdout_bytes,
        "stderrBytes": snapshot.stderr_bytes,
    })
}

fn unknown_shell<W: Write>(writer: &mut W, id: &Value, shell_id: &str) -> io::Result<()> {
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::error(format!("unknown shell session: {shell_id}")),
    )
}

fn shell_status_label(status: orca_core::task_types::TaskStatus) -> &'static str {
    match status {
        orca_core::task_types::TaskStatus::Completed => "completed",
        orca_core::task_types::TaskStatus::Stopped => "stopped",
        orca_core::task_types::TaskStatus::Failed => "failed",
        orca_core::task_types::TaskStatus::ApprovalRequired => "approval_required",
        orca_core::task_types::TaskStatus::Cancelled => "cancelled",
        orca_core::task_types::TaskStatus::Running => "running",
        orca_core::task_types::TaskStatus::Queued => "queued",
        orca_core::task_types::TaskStatus::Paused => "paused",
        orca_core::task_types::TaskStatus::Stopping => "stopping",
    }
}

fn server_cwd(config: &RunConfig) -> io::Result<PathBuf> {
    config
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
}

fn run_thread_list<W: Write>(
    state: &ServerState,
    cursor: Option<&str>,
    limit: usize,
    filters: ThreadListFilters,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    search_term: Option<&str>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let page = state.threads.list_threads(
        cursor,
        limit,
        filters,
        sort_key,
        sort_direction,
        search_term,
    )?;
    let data = page
        .data
        .into_iter()
        .map(thread_summary_to_json)
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadList {
            data: Value::from(data),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn run_thread_search<W: Write>(
    state: &ServerState,
    query: &str,
    cursor: Option<&str>,
    limit: usize,
    include_archived: bool,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if query.is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("thread search term must not be empty"),
        );
    }
    let page = state.threads.search_threads(
        query,
        cursor,
        limit,
        include_archived,
        sort_key,
        sort_direction,
    )?;
    let data = page
        .data
        .into_iter()
        .map(|hit| {
            serde_json::json!({
                "thread": thread_summary_to_json(hit.thread),
                "snippet": hit.snippet,
            })
        })
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadSearch {
            data: Value::from(data),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn thread_summary_to_json(summary: StoredThreadSummary) -> Value {
    serde_json::json!({
        "threadId": summary.thread_id,
        "title": summary.title,
        "cwd": summary.cwd,
        "provider": summary.provider,
        "model": summary.model,
        "createdAt": summary.created_at.to_rfc3339(),
        "updatedAt": summary.updated_at.to_rfc3339(),
        "archived": summary.archived,
        "parentId": summary.parent_id,
        "forked": summary.forked,
        "approvalMode": summary.approval_mode.map(|mode| mode.as_str()),
        "runtimeWorkspaceRoots": runtime_workspace_roots_to_json(summary.runtime_workspace_roots),
        "activePermissionProfile": active_permission_profile_to_json(summary.active_permission_profile),
        "permissionRuleCount": summary.permission_rule_count,
        "additionalWorkingDirectoryCount": summary.additional_working_directories.len(),
        "additionalWorkingDirectories": additional_working_directories_to_json(summary.additional_working_directories),
        "networkDomainPermissionCount": summary.network_domain_permissions.len(),
        "networkDomainPermissions": network_domain_permissions_to_json(summary.network_domain_permissions),
    })
}

fn network_domain_permissions_to_json(
    permissions: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
) -> Value {
    serde_json::to_value(permissions).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn additional_working_directories_to_json(
    directories: Vec<orca_core::config::AdditionalWorkingDirectory>,
) -> Value {
    Value::from(
        directories
            .into_iter()
            .map(|directory| {
                serde_json::json!({
                    "path": directory.path,
                    "source": directory.source,
                })
            })
            .collect::<Vec<_>>(),
    )
}

fn runtime_workspace_roots_to_json(roots: Vec<PathBuf>) -> Value {
    Value::from(
        roots
            .into_iter()
            .map(|root| Value::from(root.display().to_string()))
            .collect::<Vec<_>>(),
    )
}

fn active_permission_profile_to_json(
    profile: Option<orca_core::config::ActivePermissionProfile>,
) -> Value {
    profile
        .map(|profile| {
            serde_json::json!({
                "id": profile.id,
                "extends": profile.extends,
            })
        })
        .unwrap_or(Value::Null)
}

fn run_thread_turns_list<W: Write>(
    state: &ServerState,
    thread_id: &str,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
    items_view: TurnItemsView,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let page =
        match state
            .threads
            .list_thread_turns(thread_id, cursor, limit, sort_direction, items_view)
        {
            Some(page) => page,
            None => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("unknown thread: {thread_id}")),
                );
            }
        };

    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadTurnsList {
            data: Value::from(
                page.data
                    .into_iter()
                    .map(thread_turn_to_json)
                    .collect::<Vec<_>>(),
            ),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn run_thread_items_list<W: Write>(
    state: &ServerState,
    thread_id: &str,
    turn_id: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let page =
        match state
            .threads
            .list_thread_items(thread_id, turn_id, cursor, limit, sort_direction)
        {
            Some(page) => page,
            None => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("unknown thread: {thread_id}")),
                );
            }
        };

    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadItemsList {
            data: Value::from(
                page.data
                    .into_iter()
                    .map(thread_item_to_json)
                    .collect::<Vec<_>>(),
            ),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn optional_string_to_json(value: Option<String>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

fn run_thread_start<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    runtime_workspace_roots: Option<Vec<PathBuf>>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let mut run_config = thread_run_config(&config.run_config);
    if let Some(runtime_workspace_roots) = runtime_workspace_roots {
        run_config.runtime_workspace_roots = Some(runtime_workspace_roots);
    }
    let thread_id = state.threads.start_thread(&run_config)?;
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadStarted {
            thread_id: Value::from(thread_id),
        },
    )
}

fn run_thread_resume<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: &str,
    permissions: PermissionProfileOverride,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match state
        .threads
        .resume_thread_with_permissions(&config.run_config, thread_id, permissions)
    {
        Ok(thread_id) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadStarted {
                thread_id: Value::from(thread_id),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) => Err(error),
    }
}

fn run_thread_fork<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: &str,
    permissions: PermissionProfileOverride,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match state
        .threads
        .fork_thread_with_permissions(&config.run_config, thread_id, permissions)
    {
        Ok(thread_id) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadStarted {
                thread_id: Value::from(thread_id),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) => Err(error),
    }
}

fn run_thread_submit_async<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    id: Value,
    op: ClientOp,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    let run_config = thread_run_config(&config.run_config);
    let (thread_id, mut prompt, bindings, permissions) = match op {
        ClientOp::Submit {
            thread_id: Some(thread_id),
            prompt,
            permissions,
        } => (thread_id, prompt, None, permissions),
        ClientOp::SubmitWithMentions {
            thread_id: Some(thread_id),
            prompt,
            bindings,
            permissions,
        } => (thread_id, prompt, Some(bindings), permissions),
        _ => return Ok(()),
    };

    let Some(submission) = state.threads.submission_context(&thread_id, &permissions) else {
        return write_locked_event(
            &writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        );
    };
    if let Some(bindings) = bindings {
        match crate::mentions::expand_mentions(
            &prompt,
            &bindings,
            std::path::Path::new(&submission.cwd),
            &submission.runtime_workspace_roots,
            &submission.mcp_registry,
        ) {
            Ok(expanded) => prompt = expanded,
            Err(error) => {
                return write_locked_event(&writer, &id, ServerEvent::error(error));
            }
        }
    }
    let prepared = match state.threads.prepare_turn_with_interactions(
        &run_config,
        &thread_id,
        &prompt,
        permissions,
        &id,
        surface_adapter::JsonlInteractionTransport::new(
            state.permission_routes.clone(),
            state.direct_interactions.clone(),
        ),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return write_locked_event(&writer, &id, ServerEvent::error(error.to_string()));
        }
    };
    let active_turn_id = prepared.turn_id().clone();
    let active_thread_id = prepared.thread_id().to_string();
    let operation =
        match prepared.start_with_output(ServerTurnOutput::new(id.clone(), Arc::clone(&writer))) {
            Ok(operation) => operation,
            Err(error) => {
                return write_locked_event(&writer, &id, ServerEvent::error(error.to_string()));
            }
        };
    debug_assert_eq!(operation.turn_id(), &active_turn_id);
    debug_assert_eq!(operation.thread_id(), active_thread_id);
    state.threads.register_transport_turn(operation);
    state.prune_finished_turns();
    Ok(())
}

fn run_thread_read<W: Write>(
    state: &ServerState,
    thread_id: &str,
    include_messages: bool,
    include_turns: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let thread = match state
        .threads
        .read_thread_result(thread_id, include_messages, include_turns)
    {
        Ok(thread) => thread,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("unknown thread: {thread_id}")),
            );
        }
        Err(error) => return Err(error),
    };

    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadRead {
            thread_id: Value::from(thread.thread_id),
            title: Value::from(thread.title),
            cwd: Value::from(thread.cwd),
            runtime_workspace_roots: runtime_workspace_roots_to_json(
                thread.runtime_workspace_roots,
            ),
            active_permission_profile: active_permission_profile_to_json(
                thread.active_permission_profile,
            ),
            additional_working_directory_count: Value::from(
                thread.additional_working_directories.len() as u64,
            ),
            additional_working_directories: additional_working_directories_to_json(
                thread.additional_working_directories,
            ),
            network_domain_permission_count: Value::from(
                thread.network_domain_permissions.len() as u64
            ),
            network_domain_permissions: network_domain_permissions_to_json(
                thread.network_domain_permissions,
            ),
            message_count: Value::from(thread.message_count as u64),
            messages: Value::from(thread.messages),
            turns: Value::from(
                thread
                    .turns
                    .into_iter()
                    .map(thread_turn_to_json)
                    .collect::<Vec<_>>(),
            ),
        },
    )
}

fn run_thread_metadata_update<W: Write>(
    state: &mut ServerState,
    thread_id: &str,
    title: Option<String>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(title) = title else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("thread metadata patch did not include any supported fields"),
        );
    };

    match state.threads.update_thread_metadata_result(
        thread_id,
        ThreadMetadataPatch {
            title: Some(title.clone()),
            ..ThreadMetadataPatch::default()
        },
    ) {
        Ok(_) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadMetadataUpdated {
                thread_id: Value::from(thread_id.to_string()),
                title: Value::from(title),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

fn run_stateless_submit_async<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    id: Value,
    op: ClientOp,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    let mut run_config = config.run_config.clone();
    let (mut prompt, bindings, permissions) = match op {
        ClientOp::Submit {
            thread_id: None,
            prompt,
            permissions,
        } => (prompt, None, permissions),
        ClientOp::SubmitWithMentions {
            thread_id: None,
            prompt,
            bindings,
            permissions,
        } => (prompt, Some(bindings), permissions),
        _ => return Ok(()),
    };
    if let Some(bindings) = bindings {
        let cwd = server_cwd(&run_config)?;
        let roots = run_config
            .runtime_workspace_roots
            .clone()
            .filter(|roots| !roots.is_empty())
            .unwrap_or_else(|| vec![cwd.clone()]);
        let mcp_registry = orca_mcp::initialize_registry(&run_config.mcp_servers);
        prompt =
            match crate::mentions::expand_mentions(&prompt, &bindings, &cwd, &roots, &mcp_registry)
            {
                Ok(prompt) => prompt,
                Err(error) => {
                    return write_locked_event(&writer, &id, ServerEvent::error(error));
                }
            };
    }
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = HistoryMode::Disabled;
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;
    let prepared = match state.threads.prepare_stateless_turn_with_interactions(
        &run_config,
        &prompt,
        permissions,
        &id,
        surface_adapter::JsonlInteractionTransport::new(
            state.permission_routes.clone(),
            state.direct_interactions.clone(),
        ),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return write_locked_event(&writer, &id, ServerEvent::error(error.to_string()));
        }
    };
    let active_thread_id = prepared.thread_id().to_string();
    let operation =
        match prepared.start_with_output(ServerTurnOutput::new(id.clone(), Arc::clone(&writer))) {
            Ok(operation) => operation,
            Err(error) => {
                return write_locked_event(&writer, &id, ServerEvent::error(error.to_string()));
            }
        };
    debug_assert_eq!(operation.thread_id(), active_thread_id);
    state.threads.register_transport_turn(operation);
    state.prune_finished_turns();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_store::{SessionStore, ThreadStore};
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::event_schema::{EventDraft, EventFactory};
    use orca_core::event_sink::EventSink;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::thread_identity::TurnId;
    use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
    use std::io::{Cursor, Read};
    use tempfile::{TempDir, tempdir};

    const EOF_EVENT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

    fn emit_runtime_event<W: Write>(writer: &mut W, event: EventDraft) {
        let mut sink = EventSink::new(writer, OutputFormat::Jsonl);
        sink.emit(event).expect("serialize runtime event");
    }

    #[derive(Clone, Default)]
    struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedVecWriter {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedVecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_command_argv(script: &str) -> Vec<String> {
        if cfg!(windows) {
            vec![
                "pwsh.exe".to_string(),
                "-Command".to_string(),
                script.to_string(),
            ]
        } else {
            vec!["sh".to_string(), "-lc".to_string(), script.to_string()]
        }
    }

    fn test_command_exec_request(id: &str, script: &str, mut params: Value) -> String {
        let params = params
            .as_object_mut()
            .expect("command/exec fixture params must be an object");
        assert!(
            params
                .insert("command".to_string(), json!(test_command_argv(script)))
                .is_none(),
            "command/exec fixture command must be owned by the platform helper"
        );
        json!({"id": id, "method": "command/exec", "params": params}).to_string()
    }

    fn test_shell_script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    fn platform_slash_tmp_path() -> PathBuf {
        PathBuf::from("/tmp")
    }

    fn platform_unix_socket_path(name: &str) -> PathBuf {
        platform_slash_tmp_path().join(name)
    }

    struct DelayedTerminalWriter {
        output: SharedVecWriter,
        delay_started: Arc<std::sync::atomic::AtomicBool>,
        delay_finished: Arc<std::sync::atomic::AtomicBool>,
        delay: Duration,
    }

    impl Write for DelayedTerminalWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            use std::sync::atomic::Ordering;

            let written = self.output.write(buf)?;
            if self
                .output
                .bytes()
                .windows(b"turn_completed".len())
                .any(|window| window == b"turn_completed")
                && self
                    .delay_started
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                std::thread::sleep(self.delay);
                self.delay_finished.store(true, Ordering::Release);
            }
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ErrorAfterPendingUserInputReader {
        phase: u8,
        output: SharedVecWriter,
    }

    struct EofAfterEventReader {
        line: String,
        awaited_event: &'static str,
        output: SharedVecWriter,
        submitted: bool,
    }

    impl EofAfterEventReader {
        fn new(
            line: impl Into<String>,
            awaited_event: &'static str,
            output: SharedVecWriter,
        ) -> Self {
            Self {
                line: line.into(),
                awaited_event,
                output,
                submitted: false,
            }
        }
    }

    impl Read for EofAfterEventReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl BufRead for EofAfterEventReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(&[])
        }

        fn consume(&mut self, _amount: usize) {}

        fn read_line(&mut self, buffer: &mut String) -> io::Result<usize> {
            if !self.submitted {
                self.submitted = true;
                buffer.push_str(&self.line);
                buffer.push('\n');
                return Ok(self.line.len() + 1);
            }
            wait_for_event(&self.output.0, EOF_EVENT_WAIT_TIMEOUT, |event| {
                event["event"] == self.awaited_event
            })
            .ok_or_else(|| {
                io::Error::other(format!("{} was not emitted before EOF", self.awaited_event))
            })?;
            Ok(0)
        }
    }

    impl Read for ErrorAfterPendingUserInputReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl BufRead for ErrorAfterPendingUserInputReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(&[])
        }

        fn consume(&mut self, _amount: usize) {}

        fn read_line(&mut self, buffer: &mut String) -> io::Result<usize> {
            match self.phase {
                0 => {
                    self.phase = 1;
                    let line = r#"{"id":"thread","method":"thread/start","params":{}}"#;
                    buffer.push_str(line);
                    buffer.push('\n');
                    Ok(line.len() + 1)
                }
                1 => {
                    let started = wait_for_event(&self.output.0, Duration::from_secs(2), |event| {
                        event["event"] == "thread_started"
                    })
                    .ok_or_else(|| io::Error::other("thread did not start"))?;
                    let thread_id = started["threadId"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("missing thread id"))?;
                    self.phase = 2;
                    let line = format!(
                        r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"ask Continue?"}}]}}}}"#
                    );
                    buffer.push_str(&line);
                    buffer.push('\n');
                    Ok(line.len() + 1)
                }
                _ => {
                    wait_for_event(&self.output.0, Duration::from_secs(2), |event| {
                        event["event"] == "user_input_request"
                    })
                    .ok_or_else(|| io::Error::other("user input request did not start"))?;
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected server input failure",
                    ))
                }
            }
        }
    }

    #[test]
    fn server_input_error_releases_pending_user_input_turn() {
        let temp = tempdir().expect("tempdir");
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let output = SharedVecWriter::default();
        let reader = ErrorAfterPendingUserInputReader {
            phase: 0,
            output: output.clone(),
        };

        let error = run_with_io(ServerConfig { run_config: config }, reader, output.clone())
            .expect_err("reader failure should be returned");

        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&output.0) > 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            Arc::strong_count(&output.0),
            1,
            "server input failure must release the active turn writer"
        );
    }

    #[test]
    fn run_with_io_waits_for_runtime_host_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempdir().expect("tempdir");
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let output = SharedVecWriter::default();
        let delay_started = Arc::new(AtomicBool::new(false));
        let delay_finished = Arc::new(AtomicBool::new(false));
        let reader = ErrorAfterPendingUserInputReader {
            phase: 0,
            output: output.clone(),
        };
        let writer = DelayedTerminalWriter {
            output,
            delay_started: Arc::clone(&delay_started),
            delay_finished: Arc::clone(&delay_finished),
            delay: Duration::from_secs(2),
        };

        let error = run_with_io(ServerConfig { run_config: config }, reader, writer)
            .expect_err("reader failure should be returned");

        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert!(
            delay_started.load(Ordering::Acquire),
            "test must hold the active turn until runtime-host shutdown begins"
        );
        assert!(
            delay_finished.load(Ordering::Acquire),
            "run_with_io must wait for actor-owned generation cleanup before returning"
        );
    }

    #[test]
    fn maps_runtime_tool_events_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"tool.call.requested","payload":{"name":"read_file","target":"src/main.rs"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "tool_requested");
        assert_eq!(mapped["tool"], "read_file");
        assert_eq!(mapped["target"], "src/main.rs");
        assert!(mapped.get("type").is_none());
    }

    #[test]
    fn maps_runtime_plan_updated_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"plan.updated","payload":{"explanation":"ship it","plan":[{"step":"Inspect","status":"completed"},{"step":"Implement","status":"in_progress"}]}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(7), mapped);

        assert_eq!(mapped["event"], "turn_plan_updated");
        assert!(mapped["threadId"].is_null());
        assert!(mapped["turnId"].is_null());
        assert_eq!(mapped["explanation"], "ship it");
        assert_eq!(mapped["plan"][0]["step"], "Inspect");
        assert_eq!(mapped["plan"][0]["status"], "completed");
        assert_eq!(mapped["plan"][1]["step"], "Implement");
        assert_eq!(mapped["plan"][1]["status"], "in_progress");
    }

    #[test]
    fn maps_runtime_workflow_events_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_started");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_result_available_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.result.available","payload":{"taskId":"task-1","runId":"workflow-run-1","result":"done"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_result_available");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["result"], "done");
    }

    #[test]
    fn maps_runtime_workflow_completed_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.completed","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_completed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_failed_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","error":"boom"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_failed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["error"], "boom");
    }

    #[test]
    fn server_writer_streams_events_as_lines_arrive() {
        let mut output = Vec::new();
        let id = Value::from(42);
        let identity = ModelResponseIdentity::new(TurnId::new());
        let item_id = identity.item_ids.agent_message_item_id().to_string();
        let completed =
            CompletedModelResponse::new(identity.clone(), Some("hi".to_string()), None, Vec::new());
        let mut events = EventFactory::new("server-writer-stream".to_string());
        {
            let mut writer = ServerRequestWriter::new(id, &mut output);
            emit_runtime_event(&mut writer, events.assistant_message_delta(&identity, "hi"));
            emit_runtime_event(&mut writer, events.model_response_completed(&completed));
        }
        let events = parse_jsonl(&output);
        assert!(events.iter().all(|event| event["id"] == 42));
        assert!(events.iter().any(|event| {
            event["event"] == "item_started"
                && event["item"]["type"] == "agent_message"
                && event["item"]["id"] == item_id
        }));
        assert!(events.iter().any(|event| {
            event["event"] == "item_message_delta"
                && event["itemId"] == item_id
                && event["delta"] == "hi"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "message_delta" && event["text"] == "hi")
        );
    }

    #[test]
    fn server_writer_streams_tool_call_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"tool-1","name":"bash","target":"cargo test"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"tool-1","name":"bash","status":"completed","output":"ok","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_started");
        assert_eq!(started["item"]["tool"], "bash");
        assert_eq!(started["item"]["command"], "cargo test");
        assert_eq!(started["item"]["status"], "in_progress");

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_requested" && event["tool"] == "bash")
        );

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["aggregatedOutput"], "ok");
        assert!(completed["item"].get("output").is_none());
        assert_eq!(completed["item"]["exitCode"], 0);

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["status"] == "completed")
        );
        let legacy_completed = events
            .iter()
            .find(|event| event["event"] == "tool_completed" && event["tool"] == "bash")
            .expect("legacy tool_completed");
        assert_eq!(legacy_completed["exitCode"], 0);
    }

    #[test]
    fn server_writer_preserves_failed_command_execution_output_for_diagnostics() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"tool-1","name":"bash","target":"cargo test"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"tool-1","name":"bash","status":"failed","output":"test failure details","error":"command failed","exit_code":101}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(
            completed["item"]["aggregatedOutput"],
            "test failure details"
        );
        assert!(completed["item"].get("output").is_none());
        assert_eq!(completed["item"]["error"], "command failed");
        assert_eq!(completed["item"]["exitCode"], 101);
    }

    #[test]
    fn command_exec_manager_rejects_duplicate_active_process_id_until_removed() {
        let mut manager = CommandExecManager::default();
        let first = command_exec_process("shell-1");
        let duplicate = command_exec_process("shell-2");

        assert!(manager.insert("proc-1".to_string(), first).is_ok());
        let duplicate_error = manager
            .insert("proc-1".to_string(), duplicate)
            .expect_err("duplicate process id should be rejected");
        assert_eq!(
            duplicate_error,
            "duplicate active command/exec process id: \"proc-1\""
        );

        assert_eq!(
            manager
                .get("proc-1")
                .expect("registered process")
                .shell_id
                .as_deref(),
            Some("shell-1")
        );
        manager.remove("proc-1");
        assert!(
            manager
                .insert("proc-1".to_string(), command_exec_process("shell-3"))
                .is_ok()
        );
        assert_eq!(
            manager
                .get("proc-1")
                .expect("re-registered process")
                .shell_id
                .as_deref(),
            Some("shell-3")
        );
    }

    #[test]
    fn shell_list_does_not_reap_command_exec_owned_shells() {
        let cwd = tempdir().expect("tempdir");
        let mut state = ServerState::default();
        let handle = state
            .shells
            .spawn(
                cwd.path(),
                ShellSessionCommand {
                    command: test_shell_script(
                        "printf command-owned",
                        "[Console]::Out.Write('command-owned')",
                    ),
                    argv: None,
                    cwd: cwd.path().to_path_buf(),
                    additional_readable_directories: Vec::new(),
                    additional_working_directories: Vec::new(),
                    denied_working_directories: Vec::new(),
                    allowed_unix_socket_roots: Vec::new(),
                    env: std::collections::BTreeMap::new(),
                    description: "command exec owned shell".to_string(),
                    terminal: crate::shell_session::ShellTerminalMode::pipe(),
                    sandbox: ShellSandboxMode::DangerFullAccess,
                },
                None,
            )
            .expect("spawn command exec shell");
        state
            .command_exec
            .insert(
                "proc-shell-list".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-shell-list"),
                    command: test_command_argv("true"),
                    cwd: cwd.path().to_path_buf(),
                    denied_writable_roots: Vec::new(),
                    stream_output: false,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();
        run_shell_list(&mut state, Value::from("shell-list"), &mut output).expect("shell/list");
        drain_command_exec_processes_with_timeout(&mut state, &mut output, Duration::from_secs(4))
            .expect("drain command exec");
        let events = parse_jsonl(&output);

        assert!(
            events
                .iter()
                .all(|event| event["event"] != "shell_completed"),
            "shell/list should not complete command/exec-owned shells: {events:?}"
        );
        let listed = events
            .iter()
            .find(|event| event["event"] == "shell_listed")
            .expect("shell/list response");
        assert_eq!(
            listed["shells"].as_array().expect("shell list").len(),
            0,
            "shell/list should hide command/exec-owned shells: {events:?}"
        );
        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_completed"
                    && event["processId"] == "proc-shell-list"
                    && event["stdout"] == "command-owned"
            }),
            "command/exec owner should still emit completion after shell/list: {events:?}"
        );
    }

    #[test]
    fn command_exec_sandbox_resolves_custom_permission_profile_chain() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "locked-down".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("read-base".to_string()),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "read-base".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("locked-down".to_string()),
            ..Default::default()
        };

        let sandbox =
            test_profile_sandbox(&config, &options).expect("custom permission profile sandbox");

        assert_eq!(
            sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false
            }
        );
    }

    #[test]
    fn command_exec_sandbox_applies_custom_permission_profile_network_override() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "read-network".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                network: orca_core::config::PermissionProfileNetworkConfig {
                    enabled: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "workspace-offline".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":workspace".to_string()),
                network: orca_core::config::PermissionProfileNetworkConfig {
                    enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let read_options = protocol::CommandExecOptions {
            permission_profile: Some("read-network".to_string()),
            ..Default::default()
        };
        let workspace_options = protocol::CommandExecOptions {
            permission_profile: Some("workspace-offline".to_string()),
            ..Default::default()
        };

        let read_sandbox =
            test_profile_sandbox(&config, &read_options).expect("read-only network profile");
        let workspace_sandbox =
            test_profile_sandbox(&config, &workspace_options).expect("workspace network profile");

        assert_eq!(
            read_sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: true,
                allow_global_read: false
            }
        );
        assert_eq!(
            workspace_sandbox.mode,
            ShellSandboxMode::WorkspaceWrite {
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false
            }
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_domain_policy() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.example.com" = "allow"
"blocked.example.com" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("limited-network".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("domain policy profile");

        assert_eq!(
            sandbox.network_policy_domains.get("api.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
        assert_eq!(
            sandbox.network_policy_domains.get("blocked.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Deny)
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_unix_socket_allowlist() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.browser-socket]
extends = ":workspace"

[permission_profiles.browser-socket.network.unix_sockets]
"/tmp/orca-browser.sock" = "allow"
"/tmp/orca-blocked.sock" = "deny"
"#,
        )
        .expect("unix socket policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("browser-socket".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("unix socket policy profile");

        assert_eq!(
            sandbox.allowed_unix_socket_roots,
            vec![platform_unix_socket_path("orca-browser.sock")]
        );
    }

    #[test]
    fn command_exec_sandbox_child_domain_policy_overrides_parent_policy() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.parent]
extends = ":workspace"

[permission_profiles.parent.network.domains]
"api.example.com" = "deny"

[permission_profiles.child]
extends = "parent"

[permission_profiles.child.network.domains]
"api.example.com" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("domain policy profile");

        assert_eq!(
            sandbox.network_policy_domains.get("api.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_denied_http_request() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let input = Cursor::new(
            test_command_exec_request(
                "cmd-deny",
                "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
                json!({"permissionProfile": "limited-network", "timeoutMs": 5000}),
            )
            .into_bytes(),
        );
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let error = events
            .iter()
            .find(|event| event["event"] == "error")
            .expect("policy denial error");
        assert_eq!(
            error["message"],
            "command/exec network access to blocked.orca.invalid was denied by configured network policy"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_domain_policy_reports_blocked_host() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let input = Cursor::new(
            test_command_exec_request(
                "cmd-allowlist",
                "curl --noproxy '' -sS -D - -o /dev/null http://other.orca.invalid/ || true",
                json!({"permissionProfile": "limited-network", "timeoutMs": 5000}),
            )
            .into_bytes(),
        );
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        let stdout = completed["stdout"].as_str().expect("stdout");
        assert!(
            stdout.contains("x-proxy-error: blocked-by-allowlist"),
            "stdout should include structured proxy block reason: {completed:?}"
        );
        assert!(
            stdout.contains("x-proxy-host: other.orca.invalid"),
            "stdout should include blocked host for permission attribution: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_allowlist_miss_requests_permission_and_retries() {
        with_orca_home(|home| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let port = listener.local_addr().expect("server addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("read request") != 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 15\r\n\r\nnetwork-granted")
                    .expect("write response");
            });
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-network",
                &format!("curl --noproxy '' -sS http://127.0.0.1:{port}/"),
                json!({"threadId": thread_id, "permissionProfile": "limited-network", "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            server.join().expect("server joined");
            let retry = format!(
                r#"{{"id":"perm-allow-retry","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &retry, Arc::clone(&writer))
                .expect("idempotent permission response retry");
            let conflict = format!(
                r#"{{"id":"perm-deny-conflict","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"deny","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &conflict, Arc::clone(&writer))
                .expect("conflicting permission response retry");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "permission_resolved")
                .expect("permission resolved");
            assert_eq!(resolved["requestId"], request_id);
            assert!(events.iter().any(|event| {
                event["event"] == "permission_resolved" && event["id"] == "perm-allow-retry"
            }));
            assert!(events.iter().any(|event| {
                event["event"] == "error"
                    && event["id"] == "perm-deny-conflict"
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("different response"))
            }));
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["stdout"], "network-granted");
            assert_eq!(completed["exitCode"], 0);
            let read = crate::thread_store::SessionStore::new()
                .load_session(&thread_id)
                .expect("stored thread");
            assert_eq!(
                read.meta.network_domain_permissions.get("127.0.0.1"),
                Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
            );
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_inherited_profile_allowlist_miss_requests_permission_and_retries() {
        with_orca_home(|home| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let port = listener.local_addr().expect("server addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("read request") != 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 15\r\n\r\nnetwork-granted")
                    .expect("write response");
            });
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");
            let turn = format!(
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","activePermissionProfile":{{"id":"limited-network"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#
            );
            handle_line(&server_config, &mut state, &turn, Arc::clone(&writer))
                .expect("start profile turn");
            assert!(
                wait_for_event(&writer, Duration::from_secs(3), |event| {
                    event["event"] == "turn_completed"
                })
                .is_some(),
                "profile turn should complete"
            );

            let request = test_command_exec_request(
                "cmd-inherited-network",
                &format!("curl --noproxy '' -sS http://127.0.0.1:{port}/"),
                json!({"threadId": thread_id, "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );

            let response = format!(
                r#"{{"id":"perm-inherited-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            server.join().expect("server joined");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completions = events
                .iter()
                .filter(|event| event["event"] == "command_exec_completed")
                .collect::<Vec<_>>();
            assert_eq!(
                completions.len(),
                1,
                "inherited profile should complete once after its retry: {events:?}"
            );
            let completed = completions[0];
            assert_eq!(completed["stdout"], "network-granted");
            assert_eq!(completed["exitCode"], 0);
            let read = crate::thread_store::SessionStore::new()
                .load_session(&thread_id)
                .expect("stored thread");
            assert_eq!(
                read.meta.network_domain_permissions.get("127.0.0.1"),
                Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn command_exec_filesystem_sandbox_denial_requests_permission_and_retries() {
        assert!(
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg("(version 1) (allow default)")
                .arg("true")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false),
            "macOS Seatbelt is required for command/exec sandbox tests"
        );

        with_orca_home(|home| {
            let repo = home.join("repo");
            let git_dir = repo.join(".git");
            std::fs::create_dir_all(&git_dir).expect("git dir");
            trust_test_folder(home, &repo);
            let index_lock = git_dir.join("index.lock");
            let mut config = test_run_config();
            config.approval_mode = ApprovalMode::AutoEdit;
            config.cwd = Some(repo.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-fs",
                &format!("printf locked > {}", index_lock.display()),
                json!({"threadId": thread_id, "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["fileSystem"]["write"][0],
                git_dir.display().to_string()
            );
            assert!(
                permission_request["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains(
                        "command/exec attempted filesystem write outside the current sandbox"
                    )),
                "permission request should explain sandbox denial: {permission_request:?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
                git_dir.display()
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert_eq!(std::fs::read_to_string(&index_lock).unwrap(), "locked");
            let read = crate::thread_store::SessionStore::new()
                .load_session(&thread_id)
                .expect("stored thread");
            assert!(
                read.meta
                    .additional_working_directories
                    .iter()
                    .all(|directory| directory.path != git_dir)
            );
            assert_eq!(read.meta.metadata_writable_directories, vec![git_dir]);

            let session_target = repo.join(".git").join("session.lock");
            let session_request = test_command_exec_request(
                "cmd-fs-session",
                &format!("printf persisted > {}", session_target.display()),
                json!({"threadId": thread_id, "timeoutMs": 5000}),
            );
            handle_line(
                &server_config,
                &mut state,
                &session_request,
                Arc::clone(&writer),
            )
            .expect("session metadata command");
            assert_eq!(
                std::fs::read_to_string(session_target).expect("session metadata write"),
                "persisted"
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn command_exec_pathless_sandbox_denial_requests_unsandboxed_permission_and_retries() {
        assert!(
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg("(version 1) (allow default)")
                .arg("true")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false),
            "macOS Seatbelt is required for command/exec sandbox tests"
        );

        with_orca_home(|home| {
            let parent = sandbox_test_parent("server-unsandboxed-");
            let workspace = parent.path().join("workspace-unsandboxed");
            let outside = parent.path().join("outside-unsandboxed");
            std::fs::create_dir_all(&workspace).expect("workspace dir");
            std::fs::create_dir_all(&outside).expect("outside dir");
            trust_test_folder(home, &workspace);
            let marker = outside.join("credential-helper-output");
            let mut config = test_run_config();
            config.approval_mode = ApprovalMode::AutoEdit;
            config.cwd = Some(workspace.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let command = format!(
                "touch {} 2>/dev/null || {{ printf %s\\\\n \"fatal: could not read Username for 'https://github.com': Operation not permitted\" >&2; exit 128; }}",
                marker.display()
            );
            let request = test_command_exec_request(
                "cmd-unsandboxed",
                &command,
                json!({"threadId": thread_id, "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["shell"]["unsandboxed"],
                true
            );
            assert!(
                permission_request["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("without the filesystem sandbox")),
                "permission request should explain unsandboxed retry: {permission_request:?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"turn","permissions":{{"shell":{{"unsandboxed":true}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert!(marker.exists());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn command_exec_streaming_pathless_sandbox_denial_requests_unsandboxed_permission_and_retries()
    {
        assert!(
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg("(version 1) (allow default)")
                .arg("true")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false),
            "macOS Seatbelt is required for command/exec sandbox tests"
        );

        with_orca_home(|home| {
            let parent = sandbox_test_parent("server-unsandboxed-stream-");
            let workspace = parent.path().join("workspace-unsandboxed-stream");
            let outside = parent.path().join("outside-unsandboxed-stream");
            std::fs::create_dir_all(&workspace).expect("workspace dir");
            std::fs::create_dir_all(&outside).expect("outside dir");
            trust_test_folder(home, &workspace);
            let marker = outside.join("credential-helper-output");
            let mut config = test_run_config();
            config.approval_mode = ApprovalMode::AutoEdit;
            config.cwd = Some(workspace.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let command = format!(
                "touch {} 2>/dev/null || {{ printf %s\\\\n \"fatal: could not read Username for 'https://github.com': Operation not permitted\" >&2; exit 128; }}",
                marker.display()
            );
            let request = test_command_exec_request(
                "cmd-unsandboxed-stream",
                &command,
                json!({"threadId": thread_id, "processId": "unsandboxed-stream-1", "streamStdoutStderr": true, "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["shell"]["unsandboxed"],
                true
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"turn","permissions":{{"shell":{{"unsandboxed":true}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert!(marker.exists());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn command_exec_streaming_filesystem_sandbox_denial_requests_permission_and_retries() {
        assert!(
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg("(version 1) (allow default)")
                .arg("true")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false),
            "macOS Seatbelt is required for command/exec sandbox tests"
        );

        with_orca_home(|home| {
            let repo = home.join("repo-stream");
            let git_dir = repo.join(".git");
            std::fs::create_dir_all(&git_dir).expect("git dir");
            trust_test_folder(home, &repo);
            let index_lock = git_dir.join("index.lock");
            let mut config = test_run_config();
            config.approval_mode = ApprovalMode::AutoEdit;
            config.cwd = Some(repo.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-fs-stream",
                &format!("printf locked > {}", index_lock.display()),
                json!({"threadId": thread_id, "processId": "fs-stream-1", "streamStdoutStderr": true, "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(2),
            );
            assert!(
                events.iter().any(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "fs-stream-1"
                }),
                "streaming command should initially start: {events:?}"
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .expect("permission request");
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["fileSystem"]["write"][0],
                git_dir.display().to_string()
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
                git_dir.display()
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let starts = events
                .iter()
                .filter(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "fs-stream-1"
                })
                .count();
            assert_eq!(
                starts, 2,
                "same process id should restart after grant: {events:?}"
            );
            let completed = events
                .iter()
                .find(|event| {
                    event["event"] == "command_exec_completed"
                        && event["processId"] == "fs-stream-1"
                })
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert_eq!(std::fs::read_to_string(&index_lock).unwrap(), "locked");
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_streaming_permission_profile_block_requests_permission_and_retries_process() {
        with_orca_home(|home| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let port = listener.local_addr().expect("server addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("read request") != 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 14\r\n\r\nstream-granted")
                    .expect("write response");
            });
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-stream",
                &format!("curl --noproxy '' -sS http://127.0.0.1:{port}/"),
                json!({"threadId": thread_id, "processId": "net-stream-1", "streamStdoutStderr": true, "permissionProfile": "limited-network", "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(2),
            );
            assert!(
                events.iter().any(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "net-stream-1"
                }),
                "streaming command should initially start: {events:?}"
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .expect("permission request");
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "streaming command should wait for permission before completion: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            server.join().expect("server joined");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let starts = events
                .iter()
                .filter(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "net-stream-1"
                })
                .count();
            assert_eq!(
                starts, 2,
                "same process id should restart after grant: {events:?}"
            );
            assert!(events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["processId"] == "net-stream-1"
                    && event["stream"] == "stdout"
                    && event["delta"]
                        .as_str()
                        .is_some_and(|delta| delta.contains("stream-granted"))
            }));
            let completed = events
                .iter()
                .find(|event| {
                    event["event"] == "command_exec_completed"
                        && event["processId"] == "net-stream-1"
                })
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_streaming_permission_profile_delayed_block_requests_permission_on_next_drain() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-stream",
                "sleep 1.2; curl --noproxy '' -sS http://127.0.0.1:9/",
                json!({"threadId": thread_id, "processId": "net-stream-delayed", "streamStdoutStderr": true, "permissionProfile": "limited-network", "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "permission_request"),
                "delayed block should not be observed during initial drain: {events:?}"
            );

            let events = handle_thread_list_until_event(
                &server_config,
                &mut state,
                &writer,
                Duration::from_secs(3),
                |event| event["event"] == "permission_request",
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| {
                    panic!("permission request after delayed process drain: {events:?}")
                });
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events.iter().all(|event| {
                    !(event["event"] == "command_exec_completed"
                        && event["processId"] == "net-stream-delayed")
                }),
                "delayed block should request permission before completion: {events:?}"
            );
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_denylist_block_reports_policy_denial() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = test_command_exec_request(
                "cmd-deny",
                "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
                json!({"threadId": thread_id, "permissionProfile": "limited-network", "timeoutMs": 5000}),
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "permission_request"),
                "denylist should not be escalated into a permission request: {events:?}"
            );
            let error = events
                .iter()
                .find(|event| event["event"] == "error")
                .expect("policy denial error");
            assert_eq!(
                error["message"],
                "command/exec network access to blocked.orca.invalid was denied by configured network policy"
            );
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_domain_policy_allows_http_request() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        let port = listener.local_addr().expect("server addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nallowed")
                .expect("write response");
        });
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"127.0.0.1" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = test_command_exec_request(
            "cmd-allow",
            &format!("curl --noproxy '' -sS http://127.0.0.1:{port}/"),
            json!({"permissionProfile": "limited-network", "timeoutMs": 5000}),
        );
        let input = Cursor::new(request.into_bytes());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        server.join().expect("server joined");
        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert_eq!(completed["stdout"], "allowed");
        assert_eq!(completed["exitCode"], 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_unallowlisted_local_request() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        let port = listener.local_addr().expect("server addr").port();
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = test_command_exec_request(
            "cmd-local-deny",
            &format!("curl --noproxy '' -sS -D - -o /dev/null http://127.0.0.1:{port}/ || true"),
            json!({"permissionProfile": "limited-network", "timeoutMs": 5000}),
        );
        let input = Cursor::new(request.into_bytes());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        drop(listener);
        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert!(
            completed["stdout"]
                .as_str()
                .expect("stdout")
                .contains("x-proxy-error: blocked-by-policy"),
            "stdout should include local-network policy block: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_localhost_resolution() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = test_command_exec_request(
            "cmd-localhost-deny",
            "curl --noproxy '' -sS -D - -o /dev/null http://localhost/ || true",
            json!({"permissionProfile": "limited-network", "timeoutMs": 5000}),
        );
        let input = Cursor::new(request.into_bytes());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert!(
            completed["stdout"]
                .as_str()
                .expect("stdout")
                .contains("x-proxy-error: blocked-by-policy"),
            "stdout should include resolved localhost policy block: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[cfg(windows)]
    #[test]
    fn command_exec_permission_profile_domain_policy_fails_closed() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"127.0.0.1" = "allow"
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let input = Cursor::new(
            br#"{"id":"cmd-domain-policy","method":"command/exec","params":{"command":["powershell.exe","-NoProfile","-Command","exit 0"],"permissionProfile":"limited-network","timeoutMs":5000}}"#
                .to_vec(),
        );
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        assert_eq!(
            events.len(),
            1,
            "expected one fail-closed event: {events:?}"
        );
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "Windows domain-restricted network sandbox is unavailable; refusing to run without an OS-enforced network boundary"
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_workspace_roots() {
        let mut config = test_run_config();
        let runtime_root = std::env::current_dir().unwrap().join("runtime-root");
        let docs = runtime_root.join("docs");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":workspace_roots/docs"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            std::slice::from_ref(&runtime_root),
            None,
        )
        .expect("workspace roots profile");

        assert_eq!(sandbox.additional_writable_roots, vec![docs]);
    }

    #[test]
    fn command_exec_sandbox_collects_custom_permission_profile_read_roots() {
        let mut config = test_run_config();
        let readable = std::env::current_dir().unwrap().join("readable-root");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    readable.clone(),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read roots profile");

        assert!(sandbox.additional_readable_roots.contains(&readable));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
        assert!(sandbox.additional_writable_roots.is_empty());
    }

    #[test]
    fn command_exec_custom_read_profile_uses_strict_read_roots() {
        let mut config = test_run_config();
        let readable = std::env::current_dir()
            .unwrap()
            .join("strict-readable-root");
        config.permission_profiles.insert(
            "strict-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    readable.clone(),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("strict-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("strict read profile");

        assert_eq!(
            sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            }
        );
        assert!(sandbox.additional_readable_roots.contains(&readable));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
    }

    #[test]
    fn command_exec_sandbox_collects_custom_permission_profile_read_write_roots() {
        let mut config = test_run_config();
        let root = std::env::current_dir().unwrap().join("read-write-root");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    root.clone(),
                    orca_core::config::PermissionProfileFileAccess::ReadWrite,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read-write roots profile");

        assert!(sandbox.additional_readable_roots.contains(&root));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
        assert_eq!(sandbox.additional_writable_roots, vec![root]);
    }

    #[test]
    fn command_exec_sandbox_keeps_exact_metadata_profile_roots_separate() {
        let workspace = std::env::current_dir()
            .unwrap()
            .join("metadata-profile-workspace");
        let git_dir = workspace.join(".git");
        let git_config = git_dir.join("config");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "metadata".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":workspace".to_string()),
                filesystem: std::collections::HashMap::from([
                    (
                        git_dir.clone(),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        git_config.clone(),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        workspace.clone(),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                ])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("metadata".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(&config, &options, None, &workspace, &[], None)
            .expect("metadata profile");

        assert_eq!(sandbox.metadata_writable_roots, vec![git_dir]);
        assert_eq!(sandbox.additional_writable_roots.len(), 2);
        assert!(sandbox.additional_writable_roots.contains(&git_config));
        assert!(sandbox.additional_writable_roots.contains(&workspace));
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_deny_globs() {
        let temp = tempdir().expect("temp");
        let secret = temp.path().join("secret.env");
        let ordinary = temp.path().join("ordinary.txt");
        std::fs::write(&secret, "secret").expect("write secret");
        std::fs::write(&ordinary, "ordinary").expect("write ordinary");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "deny-env".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([
                    (
                        temp.path().to_path_buf(),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        temp.path().join("*.env"),
                        orca_core::config::PermissionProfileFileAccess::Deny,
                    ),
                ])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("deny-env".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("deny glob profile");

        assert_eq!(sandbox.additional_writable_roots, vec![temp.path()]);
        assert!(sandbox.denied_writable_roots.contains(&secret));
        assert!(!sandbox.denied_writable_roots.contains(&ordinary));
        assert!(
            !sandbox
                .denied_writable_roots
                .contains(&temp.path().to_path_buf())
        );
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_write_globs() {
        let temp = tempdir().expect("temp");
        let writable = temp.path().join("allowed.txt");
        let ordinary = temp.path().join("ordinary.md");
        std::fs::write(&writable, "allowed").expect("write allowed");
        std::fs::write(&ordinary, "ordinary").expect("write ordinary");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "write-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    temp.path().join("*.txt"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("write-glob".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("write glob profile");

        assert!(sandbox.additional_writable_roots.contains(&writable));
        assert!(!sandbox.additional_writable_roots.contains(&ordinary));
        assert!(
            !sandbox
                .additional_writable_roots
                .contains(&temp.path().to_path_buf())
        );
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_read_write_globs() {
        let temp = tempdir().expect("temp");
        let shared = temp.path().join("shared");
        let nested = shared.join("docs");
        let matched = nested.join("guide.md");
        let ignored = nested.join("image.png");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(&matched, "guide").expect("write matched");
        std::fs::write(&ignored, "image").expect("write ignored");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "rw-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    shared.join("**/*.md"),
                    orca_core::config::PermissionProfileFileAccess::ReadWrite,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("rw-glob".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read-write glob profile");

        assert!(sandbox.additional_readable_roots.contains(&matched));
        assert!(sandbox.additional_writable_roots.contains(&matched));
        assert!(!sandbox.additional_readable_roots.contains(&ignored));
        assert!(!sandbox.additional_writable_roots.contains(&ignored));
    }

    #[test]
    fn command_exec_sandbox_respects_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "shallow-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    std::collections::HashMap::from([(
                        temp.path().join("**/*.md"),
                        orca_core::config::PermissionProfileFileAccess::Read,
                    )]),
                ),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("shallow-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("shallow glob profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(!sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_inherits_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "base-depth".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    Default::default(),
                ),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "child-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("base-depth".to_string()),
                filesystem: std::collections::HashMap::from([(
                    temp.path().join("**/*.md"),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("inherited depth profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(!sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_overrides_inherited_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "base-depth".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    Default::default(),
                ),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "child-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("base-depth".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(4),
                    std::collections::HashMap::from([(
                        temp.path().join("**/*.md"),
                        orca_core::config::PermissionProfileFileAccess::Read,
                    )]),
                ),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("overridden depth profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_rejects_broad_custom_permission_profile_deny_globs() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "broad-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from("/*.env"),
                    orca_core::config::PermissionProfileFileAccess::Deny,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("broad-glob".to_string()),
            ..Default::default()
        };

        let error = test_profile_sandbox(&config, &options).expect_err("broad glob error");

        assert_eq!(
            error,
            "command/exec permissionProfile filesystem glob is too broad to scan safely: /*.env"
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_special_tmp_roots() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "tmp".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([
                    (
                        PathBuf::from(":slash_tmp"),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        PathBuf::from(":tmpdir"),
                        orca_core::config::PermissionProfileFileAccess::Deny,
                    ),
                ])
                .into(),
                ..Default::default()
            },
        );
        let tmpdir = std::env::temp_dir().join("orca-special-tmpdir");
        let options = protocol::CommandExecOptions {
            permission_profile: Some("tmp".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            Some(&tmpdir),
        )
        .expect("special tmp profile");

        assert_eq!(
            sandbox.additional_writable_roots,
            vec![platform_slash_tmp_path()]
        );
        assert_eq!(sandbox.denied_writable_roots, vec![tmpdir]);
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_root_path() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "root".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":root"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("root".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
        .expect("root profile");

        assert_eq!(sandbox.additional_writable_roots, vec![PathBuf::from("/")]);
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_minimal_path() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "minimal".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":minimal"),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("minimal".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
        .expect("minimal profile");

        assert_eq!(
            sandbox.additional_readable_roots,
            orca_tools::sandbox::platform_default_read_roots()
        );
    }

    fn assert_includes_platform_default_read_roots(actual_roots: &[PathBuf]) {
        for root in orca_tools::sandbox::platform_default_read_roots() {
            assert!(
                actual_roots.contains(&root),
                "missing platform default read root: {root:?}"
            );
        }
    }

    #[test]
    fn command_exec_sandbox_rejects_custom_permission_profile_cycle() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "a".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("b".to_string()),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "b".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("a".to_string()),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("a".to_string()),
            ..Default::default()
        };

        let error = test_profile_sandbox(&config, &options).expect_err("cycle error");

        assert_eq!(error, "command/exec permissionProfile cycle: a -> b -> a");
    }

    fn test_profile_sandbox(
        config: &RunConfig,
        options: &protocol::CommandExecOptions,
    ) -> Result<CommandExecSandbox, String> {
        command_exec_sandbox_mode(
            config,
            options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
    }

    fn command_exec_process(shell_id: &str) -> CommandExecProcess {
        CommandExecProcess {
            shell_id: Some(shell_id.to_string()),
            command_event_id: Value::from("cmd"),
            command: test_command_argv("true"),
            cwd: std::env::temp_dir(),
            denied_writable_roots: Vec::new(),
            stream_output: false,
            output_bytes_cap: None,
            output_offset: 0,
            stdout_len: 0,
            stderr_len: 0,
            stdout_cap_reached: false,
            stderr_cap_reached: false,
            network_permission_blocks: None,
            permission_request: None,
            _network_proxy: None,
        }
    }

    #[test]
    fn server_writer_streams_mcp_tool_call_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"mcp-1","name":"mcp__local__search","target":"{\"query\":\"orca\"}","raw_arguments":"{\"query\":\"orca\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"mcp-1","name":"mcp__local__search","status":"completed","output":"{\"content\":[{\"type\":\"text\",\"text\":\"found\"}],\"structuredContent\":{\"count\":1},\"_meta\":{\"source\":\"test\"}}","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_started");
        assert_eq!(started["item"]["server"], "local");
        assert_eq!(started["item"]["tool"], "search");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["query"], "orca");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["server"], "local");
        assert_eq!(completed["item"]["tool"], "search");
        assert_eq!(completed["item"]["result"]["content"][0]["text"], "found");
        assert_eq!(completed["item"]["result"]["structuredContent"]["count"], 1);
        assert_eq!(completed["item"]["result"]["_meta"]["source"], "test");
        assert!(completed["item"]["error"].is_null());
    }

    #[test]
    fn server_writer_streams_failed_mcp_tool_exit_code_in_item_error() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"mcp-1","name":"mcp__local__search","target":"{\"query\":\"orca\"}","raw_arguments":"{\"query\":\"orca\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"mcp-1","name":"mcp__local__search","status":"failed","error":"MCP request timed out","exit_code":124}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"]["result"].is_null());
        assert_eq!(
            completed["item"]["error"]["message"],
            "MCP request timed out"
        );
        assert_eq!(completed["item"]["error"]["exitCode"], 124);
    }

    #[test]
    fn server_writer_streams_external_tool_as_dynamic_tool_call_item() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-1","name":"deploy","target":"{\"env\":\"staging\"}","raw_arguments":"{\"env\":\"staging\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-1","name":"deploy","status":"completed","output":"deployed staging","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_started");
        assert!(started["item"]["namespace"].is_null());
        assert_eq!(started["item"]["tool"], "deploy");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["env"], "staging");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["success"], true);
        assert_eq!(completed["item"]["contentItems"][0]["type"], "text");
        assert_eq!(
            completed["item"]["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(completed["item"]["error"].is_null());
    }

    #[test]
    fn server_writer_streams_denied_external_tool_as_failed_dynamic_item() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-denied-1","name":"deploy","target":"{\"env\":\"production\"}","raw_arguments":"{\"env\":\"production\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-denied-1","name":"deploy","status":"denied","output":"policy denied deploy"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-denied-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "denied");
        assert_eq!(completed["item"]["success"], false);
        assert!(completed["item"]["contentItems"].is_null());
        assert_eq!(
            completed["item"]["error"]["message"],
            "policy denied deploy"
        );
    }

    #[test]
    fn server_writer_streams_failed_external_tool_exit_code_in_dynamic_item_error() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-1","name":"deploy","target":"{\"env\":\"staging\"}","raw_arguments":"{\"env\":\"staging\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-1","name":"deploy","status":"failed","error":"deploy failed","exit_code":42}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(completed["item"]["success"], false);
        assert!(completed["item"]["contentItems"].is_null());
        assert_eq!(completed["item"]["error"]["message"], "deploy failed");
        assert_eq!(completed["item"]["error"]["exitCode"], 42);
    }

    #[test]
    fn server_writer_streams_file_change_item_lifecycle_for_edit() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"completed","output":"edited note.txt","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_started");
        assert_eq!(started["item"]["status"], "inProgress");
        assert!(started["item"].get("tool").is_none());
        assert_eq!(started["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(started["item"]["changes"][0]["kind"], "edit");
        assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["tool"] == "edit")
        );
    }

    #[test]
    fn server_writer_streams_failed_file_change_item_lifecycle_for_edit() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"failed","error":"edit old text was not found","exit_code":1}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["tool"] == "edit")
        );
    }

    #[test]
    fn server_writer_streams_failed_file_change_output_as_error_detail() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"failed","output":"edit old text was not found","exit_code":1}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    }

    #[test]
    fn server_writer_streams_file_change_item_lifecycle_for_write_file() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"write-1","name":"write_file","target":"new.txt"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"write-1","name":"write_file","status":"completed","output":"wrote 3 bytes to new.txt","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "write-1:file-change"
            })
            .expect("file_change item_started");
        assert!(started["item"].get("tool").is_none());
        assert_eq!(started["item"]["status"], "inProgress");
        assert_eq!(started["item"]["changes"][0]["path"], "new.txt");
        assert_eq!(started["item"]["changes"][0]["kind"], "write");
        assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "write-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "new.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "write");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    }

    #[test]
    fn server_writer_streams_workflow_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.result.available","payload":{"taskId":"task-1","runId":"workflow-run-1","result":"done","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.completed","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"completed"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_started");
        assert_eq!(started["item"]["workflowName"], "audit");
        assert_eq!(started["item"]["taskId"], "task-1");
        assert_eq!(started["item"]["status"], "running");

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_started"),
            "workflow start was not projected: {events:#?}"
        );
        assert!(events.iter().any(|event| {
            event["event"] == "workflow_result_available" && event["result"] == "done"
        }));

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_completed");
        assert_eq!(completed["item"]["id"], started["item"]["id"]);
        assert_eq!(completed["item"]["workflowName"], "audit");
        assert_eq!(completed["item"]["taskId"], "task-1");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["result"], "done");
    }

    #[test]
    fn server_writer_streams_failed_workflow_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","error":"boom","task":{"kind":"workflow","status":"failed"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_completed");
        assert_eq!(completed["item"]["workflowName"], "audit");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(completed["item"]["error"], "boom");
        assert!(completed["item"]["result"].is_null());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_failed" && event["error"] == "boom")
        );
    }

    #[test]
    fn server_writer_streams_reasoning_item_lifecycle() {
        let mut output = Vec::new();
        let identity = ModelResponseIdentity::new(TurnId::new());
        let item_id = identity.item_ids.reasoning_item_id.to_string();
        let completed = CompletedModelResponse::new(
            identity.clone(),
            Some("answer".to_string()),
            Some("thinking".to_string()),
            Vec::new(),
        );
        let mut events = EventFactory::new("server-writer-reasoning".to_string());
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            emit_runtime_event(
                &mut writer,
                events.assistant_reasoning_delta(&identity, "thinking"),
            );
            emit_runtime_event(&mut writer, events.model_response_completed(&completed));
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "reasoning"
                    && event["item"]["id"] == item_id
            })
            .expect("reasoning item_started");
        assert_eq!(started["item"]["summary"], "");
        assert_eq!(started["item"]["content"], "");

        assert!(events.iter().any(|event| {
            event["event"] == "item_reasoning_delta"
                && event["itemId"] == item_id
                && event["delta"] == "thinking"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "reasoning_delta" && event["text"] == "thinking")
        );

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "reasoning"
                    && event["item"]["id"] == item_id
            })
            .expect("reasoning item_completed");
        assert_eq!(completed["item"]["summary"], "thinking");
        assert_eq!(completed["item"]["content"], "");
    }

    #[test]
    fn server_writer_streams_proposed_plan_item_lifecycle() {
        let mut output = Vec::new();
        let content = "Preface\n<proposed_plan>\n# Final plan\n- first\n- second\n</proposed_plan>\nPostscript";
        let identity = ModelResponseIdentity::new(TurnId::new());
        let agent_message_item_id = identity.item_ids.agent_message_item_id().to_string();
        let plan_item_id = identity.item_ids.plan_item_id.to_string();
        let completed = CompletedModelResponse::new(
            identity.clone(),
            Some(content.to_string()),
            None,
            Vec::new(),
        );
        let mut events = EventFactory::new("server-writer-plan".to_string());
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            emit_runtime_event(
                &mut writer,
                events.assistant_message_delta(&identity, content),
            );
            emit_runtime_event(&mut writer, events.model_response_completed(&completed));
        }

        let events = parse_jsonl(&output);
        let plan_started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "plan"
                    && event["item"]["id"] == plan_item_id
            })
            .expect("plan item_started");
        assert_eq!(plan_started["item"]["text"], "");

        let plan_delta = events
            .iter()
            .find(|event| event["event"] == "item_plan_delta")
            .expect("plan delta");
        assert_eq!(plan_delta["itemId"], plan_item_id);
        assert_eq!(plan_delta["delta"], "# Final plan\n- first\n- second\n");

        let plan_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "plan"
                    && event["item"]["id"] == plan_item_id
            })
            .expect("plan item_completed");
        assert_eq!(
            plan_completed["item"]["text"],
            "# Final plan\n- first\n- second\n"
        );

        let message_delta_text = events
            .iter()
            .filter(|event| event["event"] == "item_message_delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<String>();
        assert_eq!(message_delta_text, "Preface\n\nPostscript");

        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "agent_message"
                    && event["item"]["id"] == agent_message_item_id
            })
            .expect("agent message item_completed");
        assert_eq!(agent_completed["item"]["text"], "Preface\n\nPostscript");
    }

    #[test]
    fn server_writer_parses_proposed_plan_tag_split_across_deltas() {
        let mut output = Vec::new();
        let identity = ModelResponseIdentity::new(TurnId::new());
        let completed = CompletedModelResponse::new(
            identity.clone(),
            Some("Intro\n<proposed_plan>\n- Step 1\n</proposed_plan>\nOutro".to_string()),
            None,
            Vec::new(),
        );
        let mut events = EventFactory::new("server-writer-split-plan".to_string());
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            emit_runtime_event(
                &mut writer,
                events.assistant_message_delta(&identity, "Intro\n<proposed"),
            );
            emit_runtime_event(
                &mut writer,
                events.assistant_message_delta(
                    &identity,
                    "_plan>\n- Step 1\n</proposed_plan>\nOutro",
                ),
            );
            emit_runtime_event(&mut writer, events.model_response_completed(&completed));
        }

        let events = parse_jsonl(&output);
        let plan_delta = events
            .iter()
            .find(|event| event["event"] == "item_plan_delta")
            .expect("plan delta");
        assert_eq!(plan_delta["delta"], "- Step 1\n");

        let message_delta_text = events
            .iter()
            .filter(|event| event["event"] == "item_message_delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<String>();
        assert_eq!(message_delta_text, "Intro\n\nOutro");

        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
            })
            .expect("agent message item_completed");
        assert_eq!(agent_completed["item"]["text"], "Intro\n\nOutro");
    }

    #[test]
    fn server_writer_leaves_incomplete_proposed_plan_tag_as_agent_message() {
        let mut output = Vec::new();
        let content = "Intro\n<proposed_plan> not a complete block";
        let identity = ModelResponseIdentity::new(TurnId::new());
        let completed = CompletedModelResponse::new(
            identity.clone(),
            Some(content.to_string()),
            None,
            Vec::new(),
        );
        let mut events = EventFactory::new("server-writer-incomplete-plan".to_string());
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            emit_runtime_event(
                &mut writer,
                events.assistant_message_delta(&identity, content),
            );
            emit_runtime_event(&mut writer, events.model_response_completed(&completed));
        }

        let events = parse_jsonl(&output);
        assert!(
            !events
                .iter()
                .any(|event| event["event"] == "item_started" && event["item"]["type"] == "plan")
        );
        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
            })
            .expect("agent message item_completed");
        assert_eq!(
            agent_completed["item"]["text"],
            "Intro\n<proposed_plan> not a complete block"
        );
    }

    #[test]
    fn workflow_submit_streams_background_result() {
        let output = SharedVecWriter::default();
        let input = EofAfterEventReader::new(
            r#"{"id":7,"op":"submit","prompt":"workflow inline"}"#,
            "turn_completed",
            output.clone(),
        );

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server run");

        let events = parse_jsonl(&output.bytes());
        assert!(events.iter().all(|event| event["id"] == 7));
        assert!(events.iter().any(|event| {
            event["event"] == "tool_completed"
                && event["tool"] == "Workflow"
                && event["status"] == "completed"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_started"),
            "workflow start was not projected: {events:#?}"
        );
        let workflow_started = events
            .iter()
            .find(|event| event["event"] == "workflow_started")
            .expect("workflow started event");
        assert_eq!(workflow_started["task"]["kind"], "workflow");
        assert_eq!(workflow_started["task"]["status"], "running");
        let turn_completed = events
            .iter()
            .position(|event| event["event"] == "turn_completed")
            .expect("turn completed event");
        for required in [
            "workflow_started",
            "workflow_completed",
            "workflow_result_available",
        ] {
            let position = events
                .iter()
                .position(|event| event["event"] == required)
                .unwrap_or_else(|| panic!("missing {required}: {events:#?}"));
            assert!(
                position < turn_completed,
                "{required} must be projected before turn_completed"
            );
        }
        let item_completed = events
            .iter()
            .position(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "workflow"
            })
            .expect("workflow item completion");
        assert!(
            item_completed < turn_completed,
            "workflow item must complete before turn_completed"
        );
    }

    #[test]
    fn stateless_submit_clean_eof_waits_for_terminal_and_joins_the_actor() {
        let output = SharedVecWriter::default();
        let input = EofAfterEventReader::new(
            r#"{"id":8,"op":"submit","prompt":"mock_stream_delay_ms 2500"}"#,
            "turn_started",
            output.clone(),
        );
        let started_at = std::time::Instant::now();

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server clean EOF completion");

        assert!(
            started_at.elapsed() >= Duration::from_secs(2),
            "clean EOF returned before the slow stateless actor reached terminal"
        );
        assert_eq!(
            Arc::strong_count(&output.0),
            1,
            "EOF shutdown retained the stateless projection writer"
        );
        let events = parse_jsonl(&output.bytes());
        assert!(events.iter().any(|event| event["event"] == "turn_started"));
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["event"] == "turn_completed" && event["status"] == "success"
                })
                .count(),
            1,
            "clean EOF must preserve one successful terminal: {events:?}"
        );
    }

    #[test]
    fn stateless_submit_clean_eof_cancels_published_user_input_before_shutdown() {
        let output = SharedVecWriter::default();
        let input = EofAfterEventReader::new(
            r#"{"id":"input","op":"submit","prompt":"ask Continue?"}"#,
            "user_input_request",
            output.clone(),
        );

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server resolves published user input at clean EOF");

        assert_eq!(
            Arc::strong_count(&output.0),
            1,
            "EOF shutdown retained the stateless projection writer"
        );
        let events = parse_jsonl(&output.bytes());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "user_input_request"),
            "the reader must close only after physical interaction publication: {events:?}"
        );
        let completed = events
            .iter()
            .filter(|event| event["event"] == "turn_completed")
            .collect::<Vec<_>>();
        assert_eq!(
            completed.len(),
            1,
            "clean EOF must project exactly one terminal after cancelling user input: {events:?}"
        );
        assert_eq!(completed[0]["status"], "cancelled");
    }

    #[test]
    fn stateless_provider_failure_emits_one_failed_terminal_and_reaps_runtime_resources() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"provider-failure","op":"submit","prompt":"mock_provider_error"}"#,
                Arc::clone(&writer),
            )
            .expect("start stateless provider failure");
            state.join_active_turns();

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let turn_started = events
                .iter()
                .find(|event| event["id"] == "provider-failure" && event["event"] == "turn_started")
                .expect("stateless turn started");
            let task_id = turn_started["task"]["task_id"]
                .as_str()
                .expect("ephemeral task id");
            let thread_id = task_id
                .rsplit_once(":task-")
                .map(|(thread_id, _)| thread_id)
                .expect("task id binds the ephemeral thread");
            let terminal = events
                .iter()
                .filter(|event| {
                    event["id"] == "provider-failure" && event["event"] == "turn_completed"
                })
                .collect::<Vec<_>>();
            assert_eq!(terminal.len(), 1, "provider failure must have one terminal");
            assert_eq!(terminal[0]["status"], "failed");
            let provider_errors = events
                .iter()
                .filter(|event| event["id"] == "provider-failure" && event["event"] == "error")
                .collect::<Vec<_>>();
            assert_eq!(
                provider_errors.len(),
                1,
                "provider failure must preserve one error event: {events:#?}"
            );
            let error_index = events
                .iter()
                .position(|event| event == provider_errors[0])
                .expect("provider error remains in event stream");
            let terminal_index = events
                .iter()
                .position(|event| event == terminal[0])
                .expect("terminal remains in event stream");
            assert!(
                error_index < terminal_index,
                "provider error must precede its terminal: {events:#?}"
            );
            let provider_error = provider_errors[0]["message"]
                .as_str()
                .expect("provider error message");
            assert_eq!(
                provider_error, "mock provider error: api_key=<redacted>",
                "provider failure must preserve a redacted diagnostic"
            );
            assert!(
                !provider_error.contains("super-secret"),
                "provider error leaked a secret: {events:#?}"
            );
            assert!(
                state.threads.jsonl_surface(thread_id).is_none(),
                "terminal transport must release the ephemeral runtime actor"
            );
            assert_eq!(
                Arc::strong_count(&writer),
                1,
                "terminal transport worker retained its projection writer"
            );
            assert_stateless_session_is_unpersisted(&state, home);

            state
                .shutdown(JsonlSupervisorCloseTrigger::NonIo(
                    JsonlNonIoCloseTrigger::SupervisorShutdown,
                ))
                .expect("shutdown server after provider failure");
        });
    }

    #[test]
    fn stateless_adapter_shutdown_cancels_joins_and_reaps_the_in_flight_turn() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"shutdown","op":"submit","prompt":"mock_stream_delay_ms 30000"}"#,
                Arc::clone(&writer),
            )
            .expect("start slow stateless turn");
            let started = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["id"] == "shutdown" && event["event"] == "turn_started"
            })
            .expect("stateless turn started");
            let task_id = started["task"]["task_id"]
                .as_str()
                .expect("ephemeral task id");
            let thread_id = task_id
                .rsplit_once(":task-")
                .map(|(thread_id, _)| thread_id)
                .expect("task id binds the ephemeral thread")
                .to_string();
            assert!(state.threads.jsonl_surface(&thread_id).is_some());
            assert_stateless_session_is_unpersisted(&state, home);

            let shutdown_started = std::time::Instant::now();
            state
                .threads
                .shutdown()
                .expect("shutdown JSONL runtime adapter");

            assert!(
                shutdown_started.elapsed() < Duration::from_secs(3),
                "adapter shutdown did not cancel the slow stateless actor promptly"
            );
            assert!(
                state.threads.jsonl_surface(&thread_id).is_none(),
                "adapter shutdown returned before the ephemeral actor was reaped"
            );
            assert_eq!(
                Arc::strong_count(&writer),
                1,
                "adapter shutdown returned before the transport worker released its writer"
            );
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let terminals = events
                .iter()
                .filter(|event| event["id"] == "shutdown" && event["event"] == "turn_completed")
                .collect::<Vec<_>>();
            assert_eq!(terminals.len(), 1, "shutdown must emit one terminal");
            assert_eq!(terminals[0]["status"], "cancelled");
            assert_eq!(
                recorded_session_files_under(home),
                Vec::<std::path::PathBuf>::new(),
                "adapter shutdown materialized a recorded session transcript"
            );

            state
                .shutdown(JsonlSupervisorCloseTrigger::NonIo(
                    JsonlNonIoCloseTrigger::SupervisorShutdown,
                ))
                .expect("close supervisor after adapter shutdown");
        });
    }

    #[test]
    fn submit_turn_started_event_preserves_task_lifecycle_metadata() {
        let input = Cursor::new(br#"{"id":7,"op":"submit","prompt":"reply once"}"#.to_vec());
        let output = SharedVecWriter::default();

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let turn_started = events
            .iter()
            .find(|event| event["event"] == "turn_started")
            .expect("turn started event");

        assert_eq!(turn_started["turn"], 1);
        let turn_id = turn_started["turnId"].as_str().expect("logical turn id");
        assert!(turn_id.starts_with("turn_"));
        assert_eq!(turn_started["task"]["kind"], "agent");
        assert_eq!(turn_started["task"]["status"], "running");
        assert_eq!(turn_started["task"]["turn"], 1);
        assert_ne!(turn_started["task"]["task_id"], turn_id);
        assert!(
            turn_started["task"]["task_id"]
                .as_str()
                .unwrap()
                .contains(":task-1")
        );
    }

    #[test]
    fn stateless_submit_uses_non_catalogued_one_shot_surface() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let mut state = ServerState::default();

            let prepared = state
                .threads
                .prepare_stateless_turn_with_interactions(
                    &config,
                    "reply once",
                    PermissionProfileOverride::default(),
                    &serde_json::json!("stateless-submit"),
                    surface_adapter::JsonlInteractionTransport::new(
                        state.permission_routes.clone(),
                        state.direct_interactions.clone(),
                    ),
                )
                .expect("prepare stateless typed turn");
            let ephemeral_thread_id = prepared.thread_id().to_string();

            assert!(
                !state.threads.has_thread(&ephemeral_thread_id),
                "stateless submit must not enter the recorded thread catalog"
            );
            assert!(
                state
                    .threads
                    .read_session(&ephemeral_thread_id, true, true)
                    .is_err(),
                "stateless submit must not materialize persisted history"
            );

            let output = Arc::new(Mutex::new(Vec::new()));
            let operation = prepared
                .start_with_output(ServerTurnOutput::new(
                    serde_json::json!("stateless-submit"),
                    Arc::clone(&output),
                ))
                .expect("start stateless typed turn");
            state.threads.register_transport_turn(operation);
            state.join_active_turns();

            let events = parse_jsonl(&output.lock().expect("output").clone());
            assert!(events.iter().any(|event| {
                event["id"] == "stateless-submit" && event["event"] == "turn_started"
            }));
            assert!(events.iter().any(|event| {
                event["id"] == "stateless-submit" && event["event"] == "turn_completed"
            }));
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "thread_started"),
                "stateless submit must not publish recorded-thread lifecycle"
            );
        });
    }

    #[test]
    fn stateless_submit_wire_route_stays_out_of_thread_catalog() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &config,
                &mut state,
                r#"{"id":"wire-stateless","op":"submit","prompt":"reply once"}"#,
                &mut output,
            )
            .expect("route stateless wire submit");

            let events = parse_jsonl(&output);
            assert!(events.iter().any(|event| {
                event["id"] == "wire-stateless" && event["event"] == "turn_started"
            }));
            assert!(events.iter().any(|event| {
                event["id"] == "wire-stateless" && event["event"] == "turn_completed"
            }));
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "thread_started")
            );
            assert!(
                state
                    .threads
                    .list_threads(
                        None,
                        usize::MAX,
                        ThreadListFilters::active(),
                        ThreadSortKey::UpdatedAt,
                        SortDirection::Desc,
                        None,
                    )
                    .expect("list recorded threads")
                    .data
                    .is_empty(),
                "wire stateless submit must not create a recorded catalog entry"
            );
        });
    }

    #[test]
    fn stateless_submit_can_be_interrupted_by_admitted_turn_id() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"stateless-wait","op":"submit","prompt":"ask Continue?"}"#,
                Arc::clone(&writer),
            )
            .expect("start stateless interactive turn");
            let started = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "turn_started"
            })
            .expect("stateless turn started");
            let turn_id = started["turnId"]
                .as_str()
                .expect("canonical stateless turn id")
                .to_string();
            wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "user_input_request"
            })
            .expect("stateless turn paused for input");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"stateless-interrupt","method":"turn/interrupt","params":{{"turnId":"{turn_id}"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("interrupt stateless turn");
            state.join_active_turns();

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(events.iter().any(|event| {
                event["id"] == "stateless-interrupt"
                    && event["event"] == "turn_controlled"
                    && event["turnId"] == turn_id
                    && event["status"] == "interrupted"
            }));
            assert!(
                state
                    .threads
                    .list_threads(
                        None,
                        usize::MAX,
                        ThreadListFilters::active(),
                        ThreadSortKey::UpdatedAt,
                        SortDirection::Desc,
                        None,
                    )
                    .expect("list recorded threads")
                    .data
                    .is_empty()
            );
        });
    }

    #[test]
    fn stateless_submit_user_input_response_resumes_one_shot_turn() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"stateless-input","op":"submit","prompt":"ask Continue?"}"#,
                Arc::clone(&writer),
            )
            .expect("start stateless interactive turn");
            let request = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "user_input_request"
            })
            .expect("stateless user input request");
            let request_id = request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"stateless-input-response","method":"user_input/respond","params":{{"requestId":"{request_id}","answer":"yes"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("respond to stateless user input");
            state.join_active_turns();

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(events.iter().any(|event| {
                event["id"] == "stateless-input-response"
                    && event["event"] == "user_input_resolved"
                    && event["answered"] == true
            }));
            assert!(events.iter().any(|event| {
                event["id"] == "stateless-input" && event["event"] == "turn_completed"
            }));
            assert!(
                state
                    .threads
                    .list_threads(
                        None,
                        usize::MAX,
                        ThreadListFilters::active(),
                        ThreadSortKey::UpdatedAt,
                        SortDirection::Desc,
                        None,
                    )
                    .expect("list recorded threads")
                    .data
                    .is_empty()
            );
        });
    }

    #[test]
    fn thread_start_materializes_recorded_history_when_enabled() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");

            let events = parse_jsonl(&output);
            let thread_id = events
                .iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str())
                .expect("thread id")
                .to_string();

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"persist this server thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let store = crate::thread_store::SessionStore::new();
            let transcript = store.load_session("latest").expect("latest transcript");
            assert_eq!(transcript.meta.session_id, thread_id);
            assert!(transcript.messages.iter().any(|message| {
                matches!(message, Message::User { content, .. } if content == "persist this server thread")
            }));
            assert!(transcript.messages.iter().any(|message| {
                matches!(message, Message::Assistant { content: Some(content), .. } if content == "Mock runtime completed the headless harness contract.")
            }));
        });
    }

    #[test]
    fn thread_read_returns_persisted_thread_projection() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"readable server thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let events = parse_jsonl(&read_output);
            assert_eq!(events.len(), 1);
            let read = &events[0];
            assert_eq!(read["id"], "read");
            assert_eq!(read["event"], "thread_read");
            assert_eq!(read["threadId"], thread_id);
            let messages = read["messages"].as_array().expect("messages");
            assert_eq!(read["messageCount"], messages.len());
            assert!(messages.iter().any(|message| {
                message["role"] == "user" && message["content"] == "readable server thread"
            }));
            assert!(
                messages
                    .iter()
                    .any(|message| message["role"] == "assistant")
            );

            let mut turns_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read-turns","method":"thread/read","params":{{"threadId":"{thread_id}","includeTurns":true}}}}"#
                ),
                &mut turns_output,
            )
            .expect("thread read with turns");

            let turn_events = parse_jsonl(&turns_output);
            assert_eq!(turn_events.len(), 1);
            assert_eq!(turn_events[0]["event"], "thread_read");
            let turns = turn_events[0]["turns"].as_array().expect("turns");
            let projected_users = turns
                .iter()
                .flat_map(|turn| {
                    turn["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|item| item["role"] == "user")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                projected_users.len(),
                1,
                "one server turn must project one user item: {projected_users:#?}"
            );
            assert!(turns.iter().any(|turn| {
                turn["items"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["role"] == "user" && item["content"] == "readable server thread"
                    }) && items.iter().any(|item| {
                        item["type"] == "agent_message"
                            && item["text"]
                                == "Mock runtime completed the headless harness contract."
                    })
                })
            }));

            drop(state);
            let mut cold_state = ServerState::default();
            let mut cold_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut cold_state,
                &format!(
                    r#"{{"id":"cold-read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true,"includeTurns":true}}}}"#
                ),
                &mut cold_output,
            )
            .expect("cold thread read");

            let cold_events = parse_jsonl(&cold_output);
            assert_eq!(cold_events.len(), 1);
            let cold_read = &cold_events[0];
            assert_eq!(cold_read["event"], "thread_read");
            let cold_messages = cold_read["messages"].as_array().expect("cold messages");
            assert_eq!(
                cold_messages
                    .iter()
                    .filter(|message| {
                        message["role"] == "user" && message["content"] == "readable server thread"
                    })
                    .count(),
                1
            );
            let cold_users = cold_read["turns"]
                .as_array()
                .expect("cold turns")
                .iter()
                .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
                .filter(|item| {
                    item["role"] == "user" && item["content"] == "readable server thread"
                })
                .collect::<Vec<_>>();
            assert_eq!(
                cold_users.len(),
                1,
                "cold projection must preserve one canonical user item: {cold_users:#?}"
            );
        });
    }

    #[test]
    fn thread_projections_prefer_the_live_owner_over_persisted_records() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let expected_read = state
                .threads
                .read_thread(&thread_id, true, false)
                .expect("live read projection");
            let expected_turns = state
                .threads
                .list_thread_turns(
                    &thread_id,
                    None,
                    10,
                    SortDirection::Asc,
                    TurnItemsView::Full,
                )
                .expect("live turns projection");
            let expected_items = state
                .threads
                .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
                .expect("live items projection");

            let transcript = SessionStore::new()
                .load_session(&thread_id)
                .expect("load live transcript path");
            let mut persisted = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript.path)
                .expect("open persisted transcript");
            writeln!(
                persisted,
                "{}",
                serde_json::json!({
                    "type": "conversation.message",
                    "message": {
                        "role": "user",
                        "content": "disk-only stale message",
                        "pinned": false
                    }
                })
            )
            .expect("append stale user record");
            writeln!(
                persisted,
                "{}",
                serde_json::json!({
                    "type": "conversation.message",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": null,
                        "tool_calls": [{
                            "id": "disk-only-call",
                            "function_name": "bash",
                            "arguments": "{\"command\":\"printf stale\"}"
                        }],
                        "pinned": false
                    }
                })
            )
            .expect("append stale assistant record");
            persisted.flush().expect("flush stale records");

            let requests = [
                format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true}}}}"#
                ),
                format!(
                    r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":10}}}}"#
                ),
                format!(
                    r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{thread_id}","limit":10}}}}"#
                ),
            ];
            let mut projection_output = Vec::new();
            for request in requests {
                handle_line_for_test(&server_config, &mut state, &request, &mut projection_output)
                    .expect("project live thread");
            }

            let events = parse_jsonl(&projection_output);
            let read = events
                .iter()
                .find(|event| event["event"] == "thread_read")
                .expect("thread_read event");
            assert_eq!(read["messageCount"], expected_read.message_count);
            assert_eq!(read["messages"], serde_json::json!(expected_read.messages));
            let turns = events
                .iter()
                .find(|event| event["event"] == "thread_turns_list")
                .expect("thread_turns_list event");
            assert_eq!(
                turns["data"],
                serde_json::json!(
                    expected_turns
                        .data
                        .into_iter()
                        .map(thread_turn_to_json)
                        .collect::<Vec<_>>()
                )
            );
            let items = events
                .iter()
                .find(|event| event["event"] == "thread_items_list")
                .expect("thread_items_list event");
            assert_eq!(
                items["data"],
                serde_json::json!(
                    expected_items
                        .data
                        .into_iter()
                        .map(thread_item_to_json)
                        .collect::<Vec<_>>()
                )
            );
        });
    }

    #[test]
    fn thread_read_returns_in_memory_thread_projection() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"readable memory thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let events = parse_jsonl(&read_output);
            assert_eq!(events.len(), 1);
            let read = &events[0];
            assert_eq!(read["event"], "thread_read");
            assert_eq!(read["threadId"], thread_id);
            let messages = read["messages"].as_array().expect("messages");
            assert!(messages.iter().any(|message| {
                message["role"] == "user" && message["content"] == "readable memory thread"
            }));
        });
    }

    #[test]
    fn concurrent_threads_receive_distinct_active_turn_routes() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            let run_config = thread_run_config(&config);
            let mut state = ServerState::default();
            let first_thread = state
                .threads
                .start_thread(&config)
                .expect("start first thread");
            let second_thread = state
                .threads
                .start_thread(&config)
                .expect("start second thread");

            let first = state
                .threads
                .prepare_turn(
                    &run_config,
                    &first_thread,
                    "mock_stream_delay_ms 500",
                    PermissionProfileOverride::default(),
                    &serde_json::json!("first-turn"),
                )
                .expect("prepare first thread turn");
            let second = state
                .threads
                .prepare_turn(
                    &run_config,
                    &second_thread,
                    "mock_stream_delay_ms 500",
                    PermissionProfileOverride::default(),
                    &serde_json::json!("second-turn"),
                )
                .expect("prepare second thread turn");
            let first_turn_id = first.turn_id().clone();
            let second_turn_id = second.turn_id().clone();

            assert_ne!(
                first_turn_id, second_turn_id,
                "process-level active-turn routing must not reuse a per-thread message index"
            );
            let writer = Arc::new(Mutex::new(Vec::new()));
            let first_operation = first
                .start_with_output(ServerTurnOutput::new(
                    serde_json::json!("first-turn"),
                    Arc::clone(&writer),
                ))
                .expect("start first thread turn");
            let second_operation = second
                .start_with_output(ServerTurnOutput::new(
                    serde_json::json!("second-turn"),
                    Arc::clone(&writer),
                ))
                .expect("start second thread turn");
            state.threads.register_transport_turn(first_operation);
            state.threads.register_transport_turn(second_operation);

            assert_eq!(
                state
                    .threads
                    .resolve_turn_thread_id(first_turn_id.as_str())
                    .as_deref(),
                Some(first_thread.as_str())
            );
            assert_eq!(
                state
                    .threads
                    .resolve_turn_thread_id(second_turn_id.as_str())
                    .as_deref(),
                Some(second_thread.as_str())
            );
            state.join_active_turns();
        });
    }

    #[test]
    fn completed_hosted_turn_allows_next_thread_turn() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let writer = Arc::new(Mutex::new(Vec::new()));
            let first = format!(
                r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#
            );
            handle_line(&server_config, &mut state, &first, Arc::clone(&writer))
                .expect("first turn");
            let first_turn_id = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["id"] == "turn-1" && event["event"] == "turn_started"
            })
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("first logical turn id");
            let completion = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["id"] == "turn-1" && event["event"] == "turn_completed"
            });
            if completion.is_none() {
                panic!(
                    "timed out waiting for first turn completion; route={:?}, events={:?}",
                    state.threads.resolve_turn_thread_id(&first_turn_id),
                    parse_complete_jsonl(&writer.lock().expect("writer").clone()),
                );
            }

            let second = format!(
                r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#
            );
            handle_line(&server_config, &mut state, &second, Arc::clone(&writer))
                .expect("second turn");
            state.join_active_turns();
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let echoed = events
                .iter()
                .filter(|event| event["id"] == "turn-2" && event["event"] == "message_delta")
                .filter_map(|event| event["text"].as_str())
                .collect::<String>();

            assert!(
                echoed.contains("first prompt | mock_history_echo"),
                "expected second turn to see prior thread history, got: {echoed}"
            );
            assert!(
                !events.iter().any(|event| {
                    event["id"] == "turn-2"
                        && event["event"] == "error"
                        && event["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("unknown thread"))
                }),
                "second turn must not race with thread reclamation"
            );
        });
    }

    #[test]
    fn thread_metadata_update_changes_read_title() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let mut metadata_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"rename","method":"thread/metadata/update","params":{{"threadId":"{thread_id}","title":"renamed from server"}}}}"#
                ),
                &mut metadata_output,
            )
            .expect("metadata update");
            let metadata_events = parse_jsonl(&metadata_output);
            assert_eq!(metadata_events.len(), 1);
            assert_eq!(metadata_events[0]["event"], "thread_metadata_updated");
            assert_eq!(metadata_events[0]["threadId"], thread_id);
            assert_eq!(metadata_events[0]["title"], "renamed from server");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}"}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let read_events = parse_jsonl(&read_output);
            assert_eq!(read_events.len(), 1);
            assert_eq!(read_events[0]["event"], "thread_read");
            assert_eq!(read_events[0]["title"], "renamed from server");
        });
    }

    #[test]
    fn thread_list_returns_persisted_thread_summaries() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut first = store
                .create_live_thread(home, "mock", None, "first listed thread")
                .expect("create first thread");
            first.complete("success").expect("complete first");
            let mut second = store
                .create_live_thread(home, "mock", None, "second listed thread")
                .expect("create second thread");
            second.complete("success").expect("complete second");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list","method":"thread/list","params":{"limit":1}}"#,
                &mut output,
            )
            .expect("thread list");

            let events = parse_jsonl(&output);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["event"], "thread_list");
            let data = events[0]["data"].as_array().expect("thread list data");
            assert_eq!(data.len(), 1);
            let first_page_title = data[0]["title"].as_str().expect("thread title");
            assert!(matches!(
                first_page_title,
                "first listed thread" | "second listed thread"
            ));
            assert_eq!(data[0]["cwd"], home.display().to_string());
            assert_eq!(events[0]["nextCursor"], "1");
            assert_eq!(events[0]["backwardsCursor"], "0");

            let mut page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list-page","method":"thread/list","params":{"cursor":"1","limit":1}}"#,
                &mut page_output,
            )
            .expect("thread list page");

            let page_events = parse_jsonl(&page_output);
            assert_eq!(page_events.len(), 1);
            assert_eq!(page_events[0]["event"], "thread_list");
            let page_data = page_events[0]["data"]
                .as_array()
                .expect("thread list page data");
            assert_eq!(page_data.len(), 1);
            let second_page_title = page_data[0]["title"].as_str().expect("thread title");
            assert!(matches!(
                second_page_title,
                "first listed thread" | "second listed thread"
            ));
            assert_ne!(first_page_title, second_page_title);
            assert_eq!(page_events[0]["nextCursor"], Value::Null);
            assert_eq!(page_events[0]["backwardsCursor"], "1");

            let mut filtered_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list-filtered","method":"thread/list","params":{"searchTerm":"second listed","limit":10}}"#,
                &mut filtered_output,
            )
            .expect("filtered thread list");

            let filtered_events = parse_jsonl(&filtered_output);
            assert_eq!(filtered_events.len(), 1);
            assert_eq!(filtered_events[0]["event"], "thread_list");
            let filtered_data = filtered_events[0]["data"]
                .as_array()
                .expect("filtered thread list data");
            assert_eq!(filtered_data.len(), 1);
            assert_eq!(filtered_data[0]["title"], "second listed thread");
            assert_eq!(filtered_events[0]["nextCursor"], Value::Null);
        });
    }

    #[test]
    fn thread_search_returns_persisted_hits() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut thread = store
                .create_live_thread(home, "mock", None, "searchable thread")
                .expect("create thread");
            let thread_id = thread.thread_id().to_string();
            thread
                .writer_mut()
                .enter_turn(orca_core::thread_identity::TurnId::new());
            thread
                .append_items(&[Message::User {
                    content: "needle appears in this transcript".to_string(),
                    pinned: false,
                }])
                .expect("append search message");
            thread.complete("success").expect("complete thread");
            let mut second = store
                .create_live_thread(home, "mock", None, "searchable thread second")
                .expect("create second thread");
            let second_id = second.thread_id().to_string();
            second
                .writer_mut()
                .enter_turn(orca_core::thread_identity::TurnId::new());
            second
                .append_items(&[Message::User {
                    content: "needle appears again".to_string(),
                    pinned: false,
                }])
                .expect("append second search message");
            second.complete("success").expect("complete second thread");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"search","method":"thread/search","params":{"searchTerm":"needle","limit":1}}"#,
                &mut output,
            )
            .expect("thread search");

            let events = parse_jsonl(&output);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["event"], "thread_search");
            let data = events[0]["data"].as_array().expect("thread search data");
            assert_eq!(data.len(), 1);
            let first_hit_id = data[0]["thread"]["threadId"]
                .as_str()
                .expect("thread id")
                .to_string();
            assert!(first_hit_id == thread_id || first_hit_id == second_id);
            assert!(
                data[0]["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("needle"))
            );
            assert_eq!(events[0]["nextCursor"], "1");

            let mut page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"search-page","method":"thread/search","params":{"searchTerm":"needle","cursor":"1","limit":1}}"#,
                &mut page_output,
            )
            .expect("thread search page");

            let page_events = parse_jsonl(&page_output);
            assert_eq!(page_events.len(), 1);
            assert_eq!(page_events[0]["event"], "thread_search");
            let page_data = page_events[0]["data"]
                .as_array()
                .expect("thread search page data");
            assert_eq!(page_data.len(), 1);
            let second_hit_id = page_data[0]["thread"]["threadId"]
                .as_str()
                .expect("thread id")
                .to_string();
            assert!(second_hit_id == thread_id || second_hit_id == second_id);
            assert_ne!(first_hit_id, second_hit_id);
            assert_eq!(page_events[0]["nextCursor"], Value::Null);
            assert_eq!(page_events[0]["backwardsCursor"], "1");
        });
    }

    #[test]
    fn thread_turns_and_items_list_return_persisted_projection() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut thread = store
                .create_live_thread(home, "mock", None, "projected server thread")
                .expect("create thread");
            let thread_id = thread.thread_id().to_string();
            let first_turn_id = orca_core::thread_identity::TurnId::new();
            let second_turn_id = orca_core::thread_identity::TurnId::new();
            thread.writer_mut().enter_turn(first_turn_id.clone());
            thread
                .append_items(&[
                    Message::User {
                        content: "server projected user".to_string(),
                        pinned: false,
                    },
                    Message::Assistant {
                        content: Some("server projected assistant".to_string()),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        pinned: false,
                    },
                ])
                .expect("append first turn projection messages");
            thread.writer_mut().enter_turn(second_turn_id.clone());
            thread
                .append_items(&[
                    Message::User {
                        content: "server projected second user".to_string(),
                        pinned: false,
                    },
                    Message::Assistant {
                        content: Some("server projected second assistant".to_string()),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        pinned: false,
                    },
                ])
                .expect("append second turn projection messages");
            thread.complete("success").expect("complete thread");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut turns_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":10}}}}"#
                ),
                &mut turns_output,
            )
            .expect("thread turns list");

            let turn_events = parse_jsonl(&turns_output);
            assert_eq!(turn_events.len(), 1);
            assert_eq!(turn_events[0]["event"], "thread_turns_list");
            let turns = turn_events[0]["data"].as_array().expect("turn data");
            assert_eq!(turns.len(), 2);
            assert_eq!(turns[0]["turnId"], first_turn_id.as_str());
            assert_eq!(turns[0]["role"], "user");
            assert_eq!(turns[0]["itemsView"], "full");
            assert_eq!(turns[0]["items"][0]["content"], "server projected user");
            assert_eq!(
                turns[0]["items"][1]["content"],
                "server projected assistant"
            );
            assert_eq!(turn_events[0]["nextCursor"], Value::Null);
            assert_eq!(turn_events[0]["backwardsCursor"], "0");

            let mut second_turn_page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-page","method":"thread/turns/list","params":{{"threadId":"{thread_id}","cursor":"1","limit":1}}}}"#
                ),
                &mut second_turn_page_output,
            )
            .expect("second thread turns page");

            let second_turn_page_events = parse_jsonl(&second_turn_page_output);
            assert_eq!(second_turn_page_events.len(), 1);
            assert_eq!(second_turn_page_events[0]["event"], "thread_turns_list");
            let page_turns = second_turn_page_events[0]["data"]
                .as_array()
                .expect("paged turn data");
            assert_eq!(page_turns.len(), 1);
            assert_eq!(page_turns[0]["turnId"], second_turn_id.as_str());
            assert_eq!(
                page_turns[0]["items"][0]["content"],
                "server projected second user"
            );
            assert_eq!(second_turn_page_events[0]["nextCursor"], Value::Null);
            assert_eq!(second_turn_page_events[0]["backwardsCursor"], "1");

            let mut latest_turn_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-desc","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":1,"sortDirection":"desc"}}}}"#
                ),
                &mut latest_turn_output,
            )
            .expect("latest thread turns page");

            let latest_turn_events = parse_jsonl(&latest_turn_output);
            assert_eq!(latest_turn_events.len(), 1);
            assert_eq!(latest_turn_events[0]["event"], "thread_turns_list");
            let latest_turns = latest_turn_events[0]["data"]
                .as_array()
                .expect("latest turn data");
            assert_eq!(latest_turns.len(), 1);
            assert_eq!(latest_turns[0]["turnId"], second_turn_id.as_str());
            assert_eq!(
                latest_turns[0]["items"][1]["content"],
                "server projected second assistant"
            );
            assert_eq!(latest_turn_events[0]["nextCursor"], "1");

            let mut unloaded_turn_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-unloaded","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":1,"itemsView":"notLoaded"}}}}"#
                ),
                &mut unloaded_turn_output,
            )
            .expect("unloaded thread turns page");

            let unloaded_turn_events = parse_jsonl(&unloaded_turn_output);
            assert_eq!(unloaded_turn_events.len(), 1);
            assert_eq!(unloaded_turn_events[0]["event"], "thread_turns_list");
            let unloaded_turns = unloaded_turn_events[0]["data"]
                .as_array()
                .expect("unloaded turn data");
            assert_eq!(unloaded_turns.len(), 1);
            assert_eq!(unloaded_turns[0]["turnId"], first_turn_id.as_str());
            assert_eq!(unloaded_turns[0]["itemsView"], "notLoaded");
            assert_eq!(
                unloaded_turns[0]["items"].as_array().expect("items").len(),
                0
            );

            let mut items_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{thread_id}","turnId":"{first_turn_id}","limit":10}}}}"#
                ),
                &mut items_output,
            )
            .expect("thread items list");

            let item_events = parse_jsonl(&items_output);
            assert_eq!(item_events.len(), 1);
            assert_eq!(item_events[0]["event"], "thread_items_list");
            let items = item_events[0]["data"].as_array().expect("item data");
            assert_eq!(items.len(), 2);
            assert!(
                items[1]["itemId"]
                    .as_str()
                    .is_some_and(|item_id| item_id.starts_with("item_"))
            );
            assert_ne!(items[0]["itemId"], items[1]["itemId"]);
            assert_eq!(items[1]["turnId"], first_turn_id.as_str());
            assert_eq!(items[1]["item"]["content"], "server projected assistant");
            assert_eq!(item_events[0]["nextCursor"], Value::Null);
            assert_eq!(item_events[0]["backwardsCursor"], "0");

            let mut second_items_page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"items-page","method":"thread/items/list","params":{{"threadId":"{thread_id}","cursor":"2","limit":2}}}}"#
                ),
                &mut second_items_page_output,
            )
            .expect("second thread items page");

            let second_items_page_events = parse_jsonl(&second_items_page_output);
            assert_eq!(second_items_page_events.len(), 1);
            assert_eq!(second_items_page_events[0]["event"], "thread_items_list");
            let page_items = second_items_page_events[0]["data"]
                .as_array()
                .expect("paged item data");
            assert_eq!(page_items.len(), 2);
            assert!(
                page_items[0]["itemId"]
                    .as_str()
                    .is_some_and(|item_id| item_id.starts_with("item_"))
            );
            assert_eq!(page_items[0]["turnId"], second_turn_id.as_str());
            assert_eq!(
                page_items[0]["item"]["content"],
                "server projected second user"
            );
            assert_eq!(second_items_page_events[0]["nextCursor"], Value::Null);
            assert_eq!(second_items_page_events[0]["backwardsCursor"], "2");

            let mut latest_item_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"item-desc","method":"thread/items/list","params":{{"threadId":"{thread_id}","limit":1,"sortDirection":"desc"}}}}"#
                ),
                &mut latest_item_output,
            )
            .expect("latest thread items page");

            let latest_item_events = parse_jsonl(&latest_item_output);
            assert_eq!(latest_item_events.len(), 1);
            assert_eq!(latest_item_events[0]["event"], "thread_items_list");
            let latest_items = latest_item_events[0]["data"]
                .as_array()
                .expect("latest item data");
            assert_eq!(latest_items.len(), 1);
            assert!(
                latest_items[0]["itemId"]
                    .as_str()
                    .is_some_and(|item_id| item_id.starts_with("item_"))
            );
            assert_eq!(
                latest_items[0]["item"]["content"],
                "server projected second assistant"
            );
            assert_eq!(latest_item_events[0]["nextCursor"], "1");
        });
    }

    #[test]
    fn user_input_response_with_unknown_request_id_reports_error() {
        let server_config = ServerConfig {
            run_config: test_run_config(),
        };
        let mut state = ServerState::default();
        let mut output = Vec::new();

        handle_line_for_test(
            &server_config,
            &mut state,
            r#"{"id":"input-response","method":"user_input/respond","params":{"requestId":"missing-input","answer":"ship it"}}"#,
            &mut output,
        )
        .expect("user input response");

        let events = parse_jsonl(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], "input-response");
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "unknown user input request: missing-input"
        );
    }

    #[test]
    fn mcp_elicitation_response_with_unknown_request_id_reports_error() {
        let server_config = ServerConfig {
            run_config: test_run_config(),
        };
        let mut state = ServerState::default();
        let mut output = Vec::new();

        handle_line_for_test(
            &server_config,
            &mut state,
            r#"{"id":"mcp-response","method":"mcp_elicitation/respond","params":{"requestId":"mcp_elicitation:github:missing","accepted":false}}"#,
            &mut output,
        )
        .expect("mcp elicitation response");

        let events = parse_jsonl(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], "mcp-response");
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "unknown MCP elicitation request: mcp_elicitation:github:missing"
        );
    }

    #[test]
    fn turn_user_input_request_waits_for_protocol_response() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"ask Continue?"}}]}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("turn start");

            let request = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "user_input_request"
            })
            .expect("user input request");
            let request_id = request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(request["threadId"], thread_id);
            assert!(
                request["question"]
                    .as_str()
                    .is_some_and(|question| question == "Confirm: Continue?")
            );
            assert_eq!(request["choices"], json!(["yes - Continue", "no - Stop"]));

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"input-response","method":"user_input/respond","params":{{"requestId":"{request_id}","answer":"yes"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("user input response");
            state.join_active_turns();

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"input-response-retry","method":"user_input/respond","params":{{"requestId":"{request_id}","answer":"yes"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("idempotent user input response retry");
            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"input-response-conflict","method":"user_input/respond","params":{{"requestId":"{request_id}","answer":"no"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("conflicting user input response retry");

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "user_input_resolved")
                .expect("user input resolved");
            assert_eq!(resolved["id"], "input-response");
            assert_eq!(resolved["requestId"], request_id);
            assert_eq!(resolved["answered"], true);
            assert!(events.iter().any(|event| {
                event["event"] == "user_input_resolved" && event["id"] == "input-response-retry"
            }));
            assert!(events.iter().any(|event| {
                event["event"] == "error"
                    && event["id"] == "input-response-conflict"
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("different response"))
            }));
            assert!(
                events
                    .iter()
                    .any(|event| event["event"] == "turn_completed"),
                "turn should complete after user input response: {events:?}"
            );
        });
    }

    #[test]
    fn turn_mcp_elicitation_request_waits_for_protocol_response() {
        with_orca_home(|home| {
            let script = home.join("eliciting-mcp-server.js");
            std::fs::write(
                &script,
                r#"
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
let pendingToolCallId = null;

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

rl.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    send({ jsonrpc: "2.0", id: message.id, result: { capabilities: {} } });
    return;
  }
  if (message.method === "notifications/initialized") {
    return;
  }
  if (message.method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        tools: [
          {
            name: "wait",
            description: "Waits for an elicitation response",
            inputSchema: { type: "object", properties: {}, required: [] }
          }
        ]
      }
    });
    return;
  }
  if (message.method === "tools/call") {
    pendingToolCallId = message.id;
    send({
      jsonrpc: "2.0",
      id: "device-flow",
      method: "elicitation/create",
      params: {
        message: "Authorize slow wait",
        url: "https://example.test/device",
        requestedSchema: {
          type: "object",
          properties: { code: { type: "string" } },
          required: ["code"]
        }
      }
    });
    return;
  }
  if (message.id === "device-flow" && pendingToolCallId !== null) {
    const action = message.result?.action || "missing";
    const code = message.result?.content?.code || "";
    send({
      jsonrpc: "2.0",
      id: pendingToolCallId,
      result: {
        content: [{ type: "text", text: `elicitation ${action} ${code}` }],
        isError: false
      }
    });
    pendingToolCallId = null;
  }
});
"#,
            )
            .expect("write fake MCP server");

            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.mcp_servers = vec![McpServerConfig {
                name: "slow".to_string(),
                command: Some("node".to_string()),
                args: vec![script.display().to_string()],
                startup_timeout_ms: Some(2_000),
                tool_timeout_ms: Some(5_000),
                ..Default::default()
            }];
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("turn start");

            let request = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "mcp_elicitation_request"
            })
            .expect("MCP elicitation request");
            let request_id = request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(request["threadId"], thread_id);
            orca_core::thread_identity::TurnId::parse(
                request["turnId"].as_str().expect("logical turn id"),
            )
            .expect("typed logical turn id");
            assert_eq!(request["serverName"], "slow");
            assert_eq!(request["mode"], "url");
            assert_eq!(request["message"], "Authorize slow wait");
            assert_eq!(request["url"], "https://example.test/device");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"mcp-response","method":"mcp_elicitation/respond","params":{{"requestId":"{request_id}","accepted":true,"contentJson":{{"code":"ABCD-1234"}}}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("MCP elicitation response");
            state.join_active_turns();

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"mcp-response-retry","method":"mcp_elicitation/respond","params":{{"requestId":"{request_id}","accepted":true,"contentJson":{{"code":"ABCD-1234"}}}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("idempotent MCP elicitation response retry");
            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"mcp-response-conflict","method":"mcp_elicitation/respond","params":{{"requestId":"{request_id}","accepted":false}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("conflicting MCP elicitation response retry");

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "mcp_elicitation_resolved")
                .expect("MCP elicitation resolved");
            assert_eq!(resolved["id"], "mcp-response");
            assert_eq!(resolved["requestId"], request_id);
            assert_eq!(resolved["accepted"], true);
            assert!(events.iter().any(|event| {
                event["event"] == "mcp_elicitation_resolved" && event["id"] == "mcp-response-retry"
            }));
            assert!(events.iter().any(|event| {
                event["event"] == "error"
                    && event["id"] == "mcp-response-conflict"
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("different response"))
            }));
            assert!(
                events
                    .iter()
                    .any(|event| event["event"] == "turn_completed"),
                "turn should complete after MCP elicitation response: {events:?}"
            );
            assert!(
                events.iter().any(|event| event["event"] == "tool_completed"
                    && event["output"]
                        .as_str()
                        .is_some_and(|output| output.contains("elicitation accept ABCD-1234"))),
                "tool output should include the accepted elicitation content: {events:?}"
            );
        });
    }

    fn test_run_config() -> RunConfig {
        // Every test resolves ORCA_HOME to the process-wide isolated home so
        // parallel tests never contend with live `orca` processes or each
        // other's deleted temp dirs; an explicitly provided home (recovery
        // child fixture) is preserved.
        let _ = crate::history::claim_isolated_test_orca_home_if_unset();
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn assert_stateless_session_is_unpersisted(state: &ServerState, home: &std::path::Path) {
        assert!(
            state
                .threads
                .list_threads(
                    None,
                    usize::MAX,
                    ThreadListFilters::active(),
                    ThreadSortKey::UpdatedAt,
                    SortDirection::Desc,
                    None,
                )
                .expect("list recorded threads")
                .data
                .is_empty(),
            "stateless submit entered the recorded thread catalog"
        );
        assert_eq!(
            recorded_session_files_under(home),
            Vec::<std::path::PathBuf>::new(),
            "stateless submit materialized a recorded session transcript"
        );
    }

    fn recorded_session_files_under(home: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut pending = [home.join("sessions"), home.join("archive")]
            .into_iter()
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        let mut files = Vec::new();
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries {
                let entry = entry.expect("read ORCA_HOME entry");
                let file_type = entry.file_type().expect("read ORCA_HOME entry type");
                let path = entry.path();
                if file_type.is_dir() {
                    pending.push(path);
                } else if file_type.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
                        })
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }

    fn drain_until_command_exec_permission_request(
        state: &mut ServerState,
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
    ) -> Vec<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events
                .iter()
                .any(|event| event["event"] == "permission_request")
            {
                return events;
            }
            let drain_outcome = {
                let mut output = writer.lock().expect("writer");
                drain_command_exec_processes_until_output_or_timeout(
                    state,
                    &mut *output,
                    Duration::from_millis(100),
                )
                .expect("drain command/exec process")
            };
            match drain_outcome {
                CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                    let mut output = writer.lock().expect("writer");
                    request_command_exec_network_permission(state, request, block, &mut *output)
                        .expect("request network permission");
                }
                CommandExecDrainOutcome::NetworkPermissionDenied {
                    command_event_id,
                    reason,
                } => {
                    let mut output = writer.lock().expect("writer");
                    protocol::write_server_event(
                        &mut *output,
                        &command_event_id,
                        ServerEvent::error(reason),
                    )
                    .expect("write network denial");
                }
                CommandExecDrainOutcome::FileSystemPermissionRequired {
                    request,
                    diagnostic,
                } => {
                    let mut output = writer.lock().expect("writer");
                    request_command_exec_file_system_permission(
                        state,
                        request,
                        diagnostic,
                        &mut *output,
                    )
                    .expect("request file-system permission");
                }
                CommandExecDrainOutcome::Drained => {}
            }
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events
                .iter()
                .any(|event| event["event"] == "permission_request")
            {
                return events;
            }
            if std::time::Instant::now() >= deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_event(
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if let Some(event) = events.into_iter().find(|event| predicate(event)) {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handle_thread_list_until_event(
        config: &ServerConfig,
        state: &mut ServerState,
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Vec<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            handle_line(
                config,
                state,
                r#"{"id":"threads","method":"thread/list","params":{}}"#,
                Arc::clone(writer),
            )
            .expect("thread list");
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events.iter().any(&mut predicate) {
                return events;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for server event: {events:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn parse_complete_jsonl(stdout: &[u8]) -> Vec<Value> {
        let text = String::from_utf8_lossy(stdout);
        let lines = text.lines();
        let has_trailing_newline = stdout.ends_with(b"\n");
        let last_index = lines.clone().count().saturating_sub(1);
        lines
            .enumerate()
            .filter_map(|(index, line)| {
                if !has_trailing_newline && index == last_index {
                    return None;
                }
                Some(serde_json::from_str(line).expect("valid complete jsonl line"))
            })
            .collect()
    }

    #[test]
    fn parse_complete_jsonl_ignores_trailing_partial_line_while_writer_is_active() {
        let output = br#"{"event":"turn_completed"}
{"event":"message_delta","text":"partial"#;

        let events = parse_complete_jsonl(output);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "turn_completed");
    }

    #[test]
    fn replaced_generation_drops_terminal_event_until_current_generation_commits() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut writer = ServerTurnOutput::new(json!("turn"), Arc::clone(&output));
        writer
            .write_all(b"{\"type\":\"session.completed\",\"payload\":{\"status\":\"cancelled\"}}\n")
            .expect("write cancelled terminal");
        writer.finish(false).expect("drop replaced terminal");
        assert!(output.lock().expect("output").is_empty());

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut writer = ServerTurnOutput::new(json!("turn"), Arc::clone(&output));
        writer
            .write_all(b"{\"type\":\"session.completed\",\"payload\":{\"status\":\"success\"}}\n")
            .expect("write successful terminal");
        writer.finish(true).expect("commit current terminal");
        let events = parse_complete_jsonl(&output.lock().expect("output"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "turn_completed");
        assert_eq!(events[0]["status"], "success");
    }

    #[test]
    fn replaced_generation_drops_runtime_cancellation_error() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut writer = ServerTurnOutput::new(json!("turn"), Arc::clone(&output));
        writer
            .write_all(
                b"{\"version\":\"1\",\"run_id\":\"run\",\"seq\":1,\"timestamp_ms\":1,\"type\":\"error\",\"payload\":{\"message\":\"turn cancelled\"}}\n",
            )
            .expect("write cancellation error");
        writer.finish(false).expect("drop replaced error");

        assert!(output.lock().expect("output").is_empty());
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        // Provide an exclusive never-removed subdirectory of the process-wide
        // isolated home for repository/trust fixtures, and resolve ORCA_HOME
        // to it for this thread and any host spawned inside the closure.
        // `ORCA_HOME` itself is never mutated: the override is a thread-local
        // that concurrent tests, background hosts, and server threads cannot
        // observe, so they always resolve the process-wide home.
        let _guard = crate::history::lock_test_env();
        let home = crate::history::isolated_test_orca_home_subdir("with-orca-home");
        crate::history::with_test_orca_home(&home, f)
    }

    fn trust_test_folder(home: &std::path::Path, folder: &std::path::Path) {
        orca_core::config::folder_trust::set_trust_with_config_dir(
            folder,
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .expect("trust test folder");
    }

    fn sandbox_test_parent(prefix: &str) -> TempDir {
        #[cfg(target_os = "macos")]
        {
            let home = PathBuf::from(
                std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
            )
            .canonicalize()
            .expect("canonical macOS HOME");
            for root in [
                Some(platform_slash_tmp_path()),
                std::env::var_os("TMPDIR").map(PathBuf::from),
            ]
            .into_iter()
            .flatten()
            {
                let root = root.canonicalize().unwrap_or(root);
                assert!(
                    !home.starts_with(&root),
                    "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                    root.display()
                );
            }
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(home)
                .expect("sandbox parent outside temporary allow roots")
        }
        #[cfg(not(target_os = "macos"))]
        {
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("sandbox parent")
        }
    }
}
