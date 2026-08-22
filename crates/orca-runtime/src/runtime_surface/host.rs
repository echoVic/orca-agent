use std::fmt;
use std::path::{Path, PathBuf};

use crate::goal_actor::GoalRuntimeHandle;
use crate::runtime_host::{
    HostedWorkflowRequest, RuntimeHost, RuntimeHostError, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use crate::thread_store::{
    SortDirection, StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchPage,
    StoredThreadSummaryPage, StoredThreadTurnPage, ThreadListFilters, ThreadMetadataPatch,
    ThreadSortKey, TurnItemsView,
};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::goal_runtime::{GoalNextAction, GoalPauseReason, GoalRecord, GoalTurnOrigin};
use orca_core::goal_types::ThreadGoal;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus};

use super::SurfaceConnectionId;
use super::{RuntimeSurfaceHandle, RuntimeSurfaceHostHandle};

pub(crate) enum RuntimeSurfaceRecordedThreadLoadError {
    CwdMismatch,
    Runtime(RuntimeHostError),
}

/// A thread-scoped typed surface entry point.
#[derive(Clone)]
pub struct RuntimeSurfaceThreadHandle {
    runtime: RuntimeThreadHandle,
    connection_id: Option<SurfaceConnectionId>,
}

impl fmt::Debug for RuntimeSurfaceThreadHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSurfaceThreadHandle")
            .field("thread_id", &self.thread_id())
            .finish_non_exhaustive()
    }
}

fn map_goal_error(error: crate::goal_actor::GoalActorError) -> RuntimeHostError {
    RuntimeHostError::GoalControlFailed {
        message: error.to_string(),
    }
}

impl RuntimeSurfaceHostHandle {
    /// Read saved thread projections without acquiring an owner lease. Opening
    /// a selected session still happens inside `RuntimeHost::start_thread`.
    pub fn list_saved_sessions(
        limit: usize,
    ) -> std::io::Result<Vec<crate::history::SessionSummary>> {
        crate::history::list_sessions(limit)
    }

    pub fn list_saved_session_page(
        offset: usize,
        limit: usize,
        search_term: Option<&str>,
    ) -> std::io::Result<crate::history::SessionSummaryPage> {
        crate::history::list_session_page(offset, limit, false, search_term)
    }

    pub fn load_saved_session(
        selector: &str,
    ) -> std::io::Result<crate::history::SessionTranscript> {
        crate::history::load_session(selector)
    }

    pub fn rename_saved_session(selector: &str, title: &str) -> std::io::Result<PathBuf> {
        crate::history::rename_session(selector, title)
    }

    pub fn archive_saved_session(selector: &str) -> std::io::Result<PathBuf> {
        crate::history::archive_session(selector)
    }

    pub fn delete_saved_session(selector: &str) -> std::io::Result<PathBuf> {
        crate::history::delete_session(selector)
    }

    pub fn folder_is_trusted(path: &Path) -> bool {
        orca_core::config::folder_trust::is_trusted(path)
    }

    pub fn set_folder_trust(path: &Path, trusted: bool) -> Result<(), String> {
        let level = if trusted {
            orca_core::config::folder_trust::TrustLevel::Trusted
        } else {
            orca_core::config::folder_trust::TrustLevel::Untrusted
        };
        orca_core::config::folder_trust::set_trust(path, level)
    }

    pub fn save_api_key(api_key: &str) -> Result<PathBuf, String> {
        orca_core::config::file::save_api_key_checked(api_key).map_err(|error| error.to_string())
    }

    pub fn project_saved_goal(session_id: &str) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| runtime.project_thread_goal(session_id))
    }

    pub fn latest_active_saved_goal() -> Result<Option<ThreadGoal>, RuntimeHostError> {
        with_saved_goal_runtime(GoalRuntimeHandle::latest_active)
    }

    pub fn pause_saved_goal(session_id: &str, at: i64) -> Result<GoalNextAction, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| {
            runtime.pause(session_id, GoalPauseReason::User, "paused by user", at)
        })
    }

    pub fn resume_saved_goal(
        session_id: &str,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| runtime.resume(session_id, GoalTurnOrigin::Resume, at))
    }

    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.start_thread_with_request(RuntimeThreadStartRequest::new(config, title))
    }

    pub(crate) fn load_recorded_thread(
        &self,
        mut config: RunConfig,
        title: impl Into<String>,
        selector: &str,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeSurfaceRecordedThreadLoadError> {
        let transcript = crate::history::load_session(selector).map_err(|error| {
            RuntimeSurfaceRecordedThreadLoadError::Runtime(RuntimeHostError::ThreadStartFailed {
                message: error.to_string(),
            })
        })?;
        if config
            .cwd
            .as_ref()
            .is_none_or(|cwd| PathBuf::from(&transcript.meta.cwd) != *cwd)
        {
            return Err(RuntimeSurfaceRecordedThreadLoadError::CwdMismatch);
        }
        config.history_mode = HistoryMode::Resume(selector.to_string());
        let request = RuntimeThreadStartRequest::new(config, title)
            .with_preloaded(transcript)
            .with_resume_scope_replacement();
        self.start_thread_with_request(request)
            .map_err(RuntimeSurfaceRecordedThreadLoadError::Runtime)
    }

    pub fn start_thread_with_request(
        &self,
        request: RuntimeThreadStartRequest,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.runtime
            .as_ref()
            .ok_or(RuntimeHostError::HostUnavailable)?
            .start_thread_with_request(request)
            .map(|runtime| {
                RuntimeSurfaceThreadHandle::from_runtime(runtime, self.connection_id().cloned())
            })
    }

    pub(crate) fn jsonl_list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> std::io::Result<StoredThreadSummaryPage> {
        self.require_runtime()?.jsonl_list_sessions(
            cursor,
            limit,
            filters,
            sort_key,
            sort_direction,
            search_term,
        )
    }

    pub(crate) fn jsonl_search_sessions(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> std::io::Result<StoredThreadSearchPage> {
        self.require_runtime()?.jsonl_search_sessions(
            query,
            cursor,
            limit,
            include_archived,
            sort_key,
            sort_direction,
        )
    }

    pub(crate) fn jsonl_read_session(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> std::io::Result<StoredThreadProjection> {
        self.require_runtime()?
            .jsonl_read_session(thread_id, include_messages, include_turns)
    }

    pub(crate) fn jsonl_list_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> std::io::Result<StoredThreadTurnPage> {
        self.require_runtime()?.jsonl_list_turns(
            thread_id,
            cursor,
            limit,
            sort_direction,
            items_view,
        )
    }

    pub(crate) fn jsonl_list_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> std::io::Result<StoredThreadItemPage> {
        self.require_runtime()?
            .jsonl_list_items(thread_id, turn_id, cursor, limit, sort_direction)
    }

    pub(crate) fn jsonl_update_session_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> std::io::Result<()> {
        self.require_runtime()?
            .jsonl_update_session_metadata(thread_id, patch)
    }

    fn require_runtime(&self) -> std::io::Result<&RuntimeHostHandle> {
        self.runtime
            .as_ref()
            .ok_or_else(|| std::io::Error::other("runtime surface host is unavailable"))
    }

    pub(crate) fn control_jsonl_turn(
        &self,
        client: super::RuntimeSurfaceClientHandle,
        request_id: super::SurfaceRequestId,
        expected_thread_id: Option<super::SurfaceThreadId>,
        legacy_turn_id: super::LegacyTurnId,
        action: super::JsonlTurnControlAction,
    ) -> Result<super::JsonlTurnControlResult, super::SurfaceClientCommandError> {
        if self.connection_id() != client.connection_id() {
            return Err(super::SurfaceClientCommandError::Unauthorized);
        }
        self.runtime
            .as_ref()
            .ok_or(super::SurfaceClientCommandError::RuntimeUnavailable)?
            .control_jsonl_turn(
                client,
                request_id,
                expected_thread_id,
                legacy_turn_id,
                action,
            )
    }
}

fn with_saved_goal_runtime<T>(
    run: impl FnOnce(&GoalRuntimeHandle) -> Result<T, crate::goal_actor::GoalActorError>,
) -> Result<T, RuntimeHostError> {
    let (runtime, join) = GoalRuntimeHandle::open_default().map_err(map_goal_error)?;
    let result = run(&runtime).map_err(map_goal_error);
    drop(runtime);
    if join.join().is_err() {
        return Err(RuntimeHostError::GoalControlFailed {
            message: "saved Goal actor panicked during shutdown".to_string(),
        });
    }
    result
}

impl RuntimeSurfaceThreadHandle {
    fn from_runtime(
        runtime: RuntimeThreadHandle,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            runtime,
            connection_id,
        }
    }

    pub fn thread_id(&self) -> &str {
        self.runtime.thread_id()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.runtime.session_id()
    }

    pub fn is_available(&self) -> bool {
        self.runtime.is_available()
    }

    pub(crate) fn shutdown(&self) -> Result<(), RuntimeHostError> {
        self.runtime.shutdown()
    }

    pub fn surface(&self) -> RuntimeSurfaceHandle {
        self.runtime.surface()
    }

    pub fn prompt_queue(
        &self,
        action: crate::prompt_queue::PromptQueueAction,
    ) -> Result<
        crate::prompt_queue::PromptQueueSnapshot,
        crate::prompt_queue::PromptQueueMutationError,
    > {
        self.runtime.prompt_queue(action)
    }

    pub fn subscribe_prompt_queue(
        &self,
    ) -> tokio::sync::watch::Receiver<crate::prompt_queue::PromptQueueSnapshot> {
        self.runtime.subscribe_prompt_queue()
    }

    pub fn acp_surface(&self) -> Option<RuntimeSurfaceHandle> {
        self.connection_id
            .clone()
            .and_then(|connection_id| self.runtime.acp_surface_for_connection(connection_id))
    }

    pub(crate) fn jsonl_surface(&self) -> Option<RuntimeSurfaceHandle> {
        self.connection_id
            .clone()
            .and_then(|connection_id| self.runtime.jsonl_surface_for_connection(connection_id))
    }

    pub(crate) fn task_registry(&self) -> crate::tasks::TaskRegistry {
        self.runtime.task_registry()
    }

    pub(crate) fn mcp_registry(&self) -> orca_mcp::McpRegistry {
        self.runtime.mcp_registry()
    }

    pub(crate) fn jsonl_read_live_projection(
        &self,
        include_messages: bool,
        include_turns: bool,
    ) -> Result<StoredThreadProjection, RuntimeHostError> {
        self.runtime
            .jsonl_read_live_projection(include_messages, include_turns)
    }

    pub(crate) fn jsonl_list_live_turns(
        &self,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> Result<StoredThreadTurnPage, RuntimeHostError> {
        self.runtime
            .jsonl_list_live_turns(cursor, limit, sort_direction, items_view)
    }

    pub(crate) fn jsonl_list_live_items(
        &self,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> Result<StoredThreadItemPage, RuntimeHostError> {
        self.runtime
            .jsonl_list_live_items(turn_id, cursor, limit, sort_direction)
    }

    pub fn read_history(&self) -> Result<Vec<super::SurfaceHistoryMessage>, RuntimeHostError> {
        self.runtime.read_surface_history()
    }

    /// Expand an input's immutable mention bindings using the registry owned by
    /// the runtime thread. TUI and other clients do not receive the registry.
    pub fn expand_mentions(
        &self,
        input: &str,
        bindings: &crate::mentions::MentionBindings,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<String, String> {
        crate::mentions::expand_mentions(
            input,
            bindings,
            cwd,
            workspace_roots,
            &self.runtime.mcp_registry(),
        )
    }

    pub fn expand_mentions_for_model(
        &self,
        input: &str,
        bindings: &crate::mentions::MentionBindings,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<crate::mentions::ExpandedPrompt, String> {
        crate::mentions::expand_mentions_for_model(
            input,
            bindings,
            cwd,
            workspace_roots,
            &self.runtime.mcp_registry(),
        )
    }

    /// Discover immutable mention candidates with the runtime-owned MCP
    /// registry. Surface clients receive the result, never the registry.
    pub fn discover_mention_catalog(&self, roots: &[PathBuf]) -> crate::mentions::MentionCatalog {
        crate::mentions::MentionCatalog::discover(roots, &self.runtime.mcp_registry())
    }

    pub fn backtrack_last_user(&self) -> Result<Option<String>, RuntimeHostError> {
        self.runtime.backtrack_last_user()
    }

    fn with_goal<T>(
        &self,
        run: impl FnOnce(&GoalRuntimeHandle) -> Result<T, crate::goal_actor::GoalActorError>,
    ) -> Result<T, RuntimeHostError> {
        let runtime = self.runtime.goal_runtime()?;
        run(&runtime).map_err(map_goal_error)
    }

    pub fn project_goal(&self, session_id: &str) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.with_goal(|runtime| runtime.project_thread_goal(session_id))
    }

    pub fn read_goal(&self, session_id: &str) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.with_goal(|runtime| runtime.read(session_id))
    }

    pub fn set_goal(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
    ) -> Result<ThreadGoal, RuntimeHostError> {
        self.runtime.set_goal(session_id, objective, at)
    }

    pub fn edit_goal(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
    ) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.runtime.edit_goal(session_id, objective, at)
    }

    pub fn clear_goal(&self, session_id: &str) -> Result<(), RuntimeHostError> {
        self.runtime.clear_goal(session_id)
    }

    pub fn pause_goal(&self, session_id: &str, at: i64) -> Result<(), RuntimeHostError> {
        self.with_goal(|runtime| {
            runtime
                .pause(session_id, GoalPauseReason::User, "paused by user", at)
                .map(|_| ())
        })
    }

    pub fn resume_goal(&self, session_id: &str, at: i64) -> Result<(), RuntimeHostError> {
        self.with_goal(|runtime| {
            runtime
                .resume(session_id, GoalTurnOrigin::Resume, at)
                .map(|_| ())
        })
    }

    pub fn resume_goal_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.with_goal(|runtime| runtime.resume_into(source_session_id, resumed_session_id, at))
    }

    pub fn task_summaries(&self) -> Vec<BackgroundTaskSummary> {
        self.runtime.task_registry().list()
    }

    pub fn stop_task(&self, task_id: &str) -> Result<Vec<BackgroundTaskSummary>, String> {
        let registry = self.runtime.task_registry();
        let task = registry
            .get(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;
        if matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Stopped
        ) {
            return Err(format!(
                "task '{task_id}' is already {}",
                task_status_label(task.status)
            ));
        }
        if task.status == TaskStatus::ApprovalRequired {
            registry.stop(task_id, "Task stopped".to_string())?;
        } else {
            registry.request_stop(task_id)?;
        }
        Ok(registry.list())
    }

    pub fn foreground_task(&self, task_id: &str) -> Result<Vec<BackgroundTaskSummary>, String> {
        let registry = self.runtime.task_registry();
        registry.mark_foregrounded(task_id)?;
        Ok(registry.list())
    }

    pub fn resolve_background_approval(
        &self,
        approval_id: &str,
        approved: bool,
    ) -> Result<(String, Vec<BackgroundTaskSummary>), String> {
        let registry = self.runtime.task_registry();
        let task_id =
            registry.submit_pending_tool_approval_response_by_request_id(approval_id, approved)?;
        if !approved {
            registry.finish_denied_pending_tool_approval(&task_id)?;
        }
        Ok((task_id, registry.list()))
    }

    pub fn launch_workflow(&self, request: HostedWorkflowRequest) -> Result<(), RuntimeHostError> {
        self.runtime.launch_workflow(request).map(|_| ())
    }

    pub fn remember_user(&self, note: &str) -> Result<PathBuf, String> {
        crate::memory::remember_user(note)
    }

    pub fn remember_project(&self, root: &Path, note: &str) -> Result<PathBuf, String> {
        crate::memory::remember_project(root, note)
    }
}

impl RuntimeThreadHandle {
    pub fn typed_surface(&self) -> RuntimeSurfaceThreadHandle {
        RuntimeSurfaceThreadHandle::from_runtime(self.clone(), None)
    }
}

impl RuntimeHost {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.handle())
    }
}

impl RuntimeHostHandle {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.clone())
    }
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Stopped => "stopped",
    }
}
