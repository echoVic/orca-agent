use crossbeam_channel as mpsc;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::RunConfig;
use orca_core::goal_types::ThreadGoal;
use orca_runtime::mentions::{ExpandedPrompt, MentionBindings, MentionCatalog};
use orca_runtime::runtime_host::HostedTurnRequest;
use orca_runtime::surface::{
    NonEmptyVec, RuntimeSettingsPatch, RuntimeSurfaceThreadHandle, SurfaceSettingsSnapshot,
    SurfaceSnapshot,
};

use crate::hosted_runtime::TuiHostedOperationOutcome;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::surface_projection::SurfaceProjectionState;
use crate::types::{TuiEvent, TuiMemoryScope};

#[cfg(test)]
static RENAME_SAVED_SESSION_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn inject_rename_saved_session_failure_once(session_id: &str) {
    *RENAME_SAVED_SESSION_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(session_id.to_string());
}

/// The only TUI-facing entry point for thread-scoped runtime commands and
/// authoritative reads. Presentation modules receive this facade instead of a
/// runtime thread handle and cannot reach runtime-owned registries or stores.
#[derive(Clone, Debug)]
pub(crate) struct TuiSurfaceActions {
    thread: RuntimeSurfaceThreadHandle,
}

pub(crate) struct TuiHostActions;

impl TuiHostActions {
    pub(crate) fn folder_is_trusted(path: &Path) -> bool {
        orca_runtime::surface::RuntimeSurfaceHostHandle::folder_is_trusted(path)
    }

    pub(crate) fn set_folder_trust(path: &Path, trusted: bool) -> Result<(), String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::set_folder_trust(path, trusted)
    }

    pub(crate) fn save_api_key(api_key: &str) -> Result<PathBuf, String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::save_api_key(api_key)
    }

    pub(crate) fn rename_saved_session(session_id: &str, title: &str) -> Result<(), String> {
        #[cfg(test)]
        {
            let mut injected = RENAME_SAVED_SESSION_FAILURE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if injected.as_deref() == Some(session_id) {
                *injected = None;
                return Err("injected saved-session rename failure".to_string());
            }
        }
        orca_runtime::surface::RuntimeSurfaceHostHandle::rename_saved_session(session_id, title)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn archive_saved_session(session_id: &str) -> Result<(), String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::archive_saved_session(session_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn delete_saved_session(session_id: &str) -> Result<(), String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::delete_saved_session(session_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl TuiSurfaceActions {
    pub(crate) fn new(thread: RuntimeSurfaceThreadHandle) -> Self {
        Self { thread }
    }

    pub(crate) fn run_turn(
        &self,
        request: HostedTurnRequest,
        config: RunConfig,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::run(&self.thread, request, config, control, event_tx)
    }

    pub(crate) fn resume_operation(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_recovered_operation(
            &self.thread,
            operation_id,
            control,
            event_tx,
        )
    }

    pub(crate) fn cancel_operation(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::cancel_recovered_operation(
            &self.thread,
            operation_id,
            control,
            event_tx,
        )
    }

    pub(crate) fn manual_compact(
        &self,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::manual_compact(&self.thread, control, event_tx)
    }

    pub(crate) fn update_settings(
        &self,
        patches: NonEmptyVec<RuntimeSettingsPatch>,
    ) -> io::Result<SurfaceSettingsSnapshot> {
        crate::surface_client::update_settings(&self.thread, patches)
    }

    pub(crate) fn read_snapshot(&self) -> io::Result<SurfaceSnapshot> {
        crate::surface_client::read_snapshot(&self.thread)
    }

    pub(crate) fn rename_current_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> io::Result<SurfaceProjectionState> {
        let before = self.read_snapshot()?;
        let committed = crate::surface_client::update_session_metadata(
            &self.thread,
            before.thread.metadata_revision,
            orca_runtime::surface::SessionMetadataPatch::SetTitle {
                title: orca_runtime::surface::DisplayText::new(title),
            },
        )
        .map_err(io::Error::other)?;

        if let Err(error) = TuiHostActions::rename_saved_session(session_id, title) {
            let compensation = crate::surface_client::update_session_metadata(
                &self.thread,
                committed.metadata_revision,
                orca_runtime::surface::SessionMetadataPatch::SetTitle {
                    title: before.thread.title.clone(),
                },
            );
            let detail = match compensation {
                Ok(_) => String::new(),
                Err(compensation) if compensation.is_stale() => {
                    "; runtime compensation rejected because session metadata changed concurrently"
                        .to_string()
                }
                Err(compensation) => {
                    format!("; runtime compensation failed: {compensation}")
                }
            };
            return Err(io::Error::other(format!(
                "failed to persist conversation rename: {error}{detail}"
            )));
        }
        let committed_cursor = committed.thread_cursor;
        let snapshot = self.read_snapshot().map_err(|error| {
            io::Error::other(format!(
                "Session rename committed but TUI projection failed: {error}"
            ))
        })?;
        if snapshot.cursor.thread_id != committed_cursor.thread_id
            || snapshot.cursor.incarnation != committed_cursor.incarnation
            || snapshot.cursor.next_seq < committed_cursor.next_seq
        {
            return Err(io::Error::other(
                "Session rename committed but TUI projection failed: snapshot did not cover the committed cursor",
            ));
        }
        let projection = SurfaceProjectionState::from_surface_snapshot(&snapshot);
        if projection.session_id.as_deref() != Some(session_id) {
            return Err(io::Error::other(
                "Session rename committed but TUI projection failed: snapshot identity did not match the recorded session",
            ));
        }
        Ok(projection.with_session_presentation(
            crate::surface_projection::SessionProjectionPresentation::Renamed,
        ))
    }

    pub(crate) fn add_pinned_context(&self, note: &str) -> io::Result<()> {
        crate::surface_client::add_pinned_context(&self.thread, note)
    }

    pub(crate) fn expand_mentions(
        &self,
        input: &str,
        bindings: &MentionBindings,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<ExpandedPrompt, String> {
        self.thread
            .expand_mentions_for_model(input, bindings, cwd, workspace_roots)
    }

    pub(crate) fn discover_mention_catalog(&self, roots: &[PathBuf]) -> MentionCatalog {
        self.thread.discover_mention_catalog(roots)
    }

    pub(crate) fn backtrack_last_user(&self) -> Result<Option<String>, String> {
        self.thread
            .backtrack_last_user()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn goal(&self, session_id: &str) -> Result<Option<ThreadGoal>, String> {
        let _ = session_id;
        crate::surface_client::read_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn edit_goal_with_committed(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
        committed: impl FnOnce(),
    ) -> Result<SurfaceProjectionState, String> {
        let _ = (session_id, at);
        crate::surface_client::edit_goal_with_committed(&self.thread, objective, committed)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn clear_goal(&self, session_id: &str) -> Result<SurfaceProjectionState, String> {
        let _ = session_id;
        crate::surface_client::clear_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn pause_goal(&self) -> Result<SurfaceProjectionState, String> {
        crate::surface_client::pause_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn set_goal_and_run_with_committed(
        &self,
        objective: String,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
        committed: impl FnOnce(),
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::set_goal_and_run_with_committed(
            &self.thread,
            objective,
            control,
            event_tx,
            committed,
        )
    }

    pub(crate) fn resume_goal_and_run(
        &self,
        prompt: String,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_goal_and_run(&self.thread, prompt, control, event_tx)
    }

    pub(crate) fn resume_goal_and_run_multimodal(
        &self,
        prompt: String,
        images: Vec<orca_core::conversation::ImageInput>,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_goal_and_run_multimodal(
            &self.thread,
            prompt,
            images,
            control,
            event_tx,
        )
    }

    pub(crate) fn resume_goal_and_run_with_started(
        &self,
        prompt: String,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
        started: impl FnOnce(),
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_goal_and_run_with_started(
            &self.thread,
            prompt,
            control,
            event_tx,
            started,
        )
    }

    pub(crate) fn recoverable_background_approval_projection(
        &self,
    ) -> Result<(SurfaceProjectionState, Vec<String>), String> {
        let snapshot = crate::surface_client::read_snapshot(&self.thread)
            .map_err(|error| error.to_string())?;
        let tools = snapshot
            .interactions
            .iter()
            .filter_map(|interaction| {
                let orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval {
                    tool,
                    ..
                } = &interaction.request
                else {
                    return None;
                };
                matches!(
                    interaction.lifecycle,
                    orca_runtime::surface::SurfaceInteractionLifecycle::Requested
                )
                .then(|| tool.name.as_str().to_string())
            })
            .collect();
        Ok((
            SurfaceProjectionState::from_surface_snapshot(&snapshot),
            tools,
        ))
    }

    pub(crate) fn stop_task(
        &self,
        task_id: &str,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<SurfaceProjectionState, String> {
        crate::surface_client::stop_task(&self.thread, task_id, control, event_tx)
    }

    pub(crate) fn foreground_task(
        &self,
        task_id: &str,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<SurfaceProjectionState, String> {
        crate::surface_client::foreground_task(&self.thread, task_id, control, event_tx)
    }

    pub(crate) fn resolve_background_approval(
        &self,
        approval_id: &str,
        approved: bool,
        control: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<(String, SurfaceProjectionState), String> {
        let (task_id, tasks) = crate::surface_client::resolve_background_approval(
            &self.thread,
            approval_id,
            approved,
            control,
            event_tx,
        )?;
        Ok((task_id, tasks))
    }

    pub(crate) fn launch_workflow(
        &self,
        name: &str,
        args: Option<&str>,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<(), String> {
        crate::surface_client::launch_workflow(&self.thread, name, args, event_tx)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn remember(
        &self,
        scope: TuiMemoryScope,
        cwd: &Path,
        note: &str,
    ) -> Result<PathBuf, String> {
        match scope {
            TuiMemoryScope::User => self.thread.remember_user(note),
            TuiMemoryScope::Project => self.thread.remember_project(cwd, note),
        }
    }
}
