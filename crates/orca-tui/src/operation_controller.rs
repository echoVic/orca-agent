use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    io,
};

use crossbeam_channel::Sender;
use orca_core::cancel::{OperationId, OperationIdAllocator};

use crate::surface_projection::TuiStreamDeliveryWatermark;
use crate::types::{TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};

/// Presentation-side correlation for a typed runtime surface operation.
///
/// This handle intentionally exposes no legacy operation handle, interaction
/// broker, or generation control to the typed surface client.
#[derive(Clone, Debug)]
pub(crate) struct TuiSurfaceTaskControl {
    hosted: Arc<HostedOperationState>,
    surface_ids: Arc<OperationIdAllocator>,
}

impl TuiSurfaceTaskControl {
    pub(crate) fn new() -> Self {
        Self {
            hosted: Arc::new(HostedOperationState::default()),
            surface_ids: Arc::new(OperationIdAllocator::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn isolated_for_test() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct HostedOperationState {
    inner: Mutex<HostedOperationInner>,
    surface_presentation_transition: Mutex<()>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct HostedOperationInner {
    surface_active: Option<SurfaceActiveOperation>,
    surface_presentation_tasks: Vec<SurfacePresentationTask>,
    surface_activation_armed: bool,
    interrupt_requested: bool,
    background_requested: bool,
    surface_delivery_watermarks:
        HashMap<orca_runtime::surface::SurfaceOperationId, TuiStreamDeliveryWatermark>,
    surface_terminal_deliveries: HashSet<orca_runtime::surface::SurfaceOperationId>,
    queue_event_tx: Arc<Mutex<Option<Sender<TuiEvent>>>>,
    queue_runtime: Option<orca_runtime::runtime_host::RuntimeThreadHandle>,
    queue_watcher_stop: Option<Arc<AtomicBool>>,
    queue_interactions:
        HashMap<TuiInteractionKey, std::sync::mpsc::SyncSender<io::Result<TuiInteractionResponse>>>,
    shutdown: bool,
}

#[derive(Debug)]
struct SurfacePresentationTask {
    operation_id: orca_runtime::surface::SurfaceOperationId,
    cancelled: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

#[derive(Clone, Debug)]
pub(crate) struct SurfacePresentationCancellation {
    cancelled: Arc<AtomicBool>,
}

impl SurfacePresentationCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg_attr(test, allow(dead_code))]
impl TuiSurfaceTaskControl {
    #[cfg(test)]
    pub(crate) fn current_id(&self) -> Option<OperationId> {
        self.lock_hosted()
            .surface_active
            .as_ref()
            .map(|operation| operation.ui_operation_id)
    }

    #[cfg(test)]
    pub(crate) fn has_pending_interrupt(&self) -> bool {
        self.lock_hosted().interrupt_requested
    }

    pub(crate) fn interrupt_current(&self) -> io::Result<bool> {
        let mut hosted = self.lock_hosted();
        if cancel_surface_if_active_checked(&mut hosted)? {
            return Ok(true);
        }
        let queue_runtime = hosted.queue_runtime.clone();
        drop(hosted);
        let queue_snapshot = queue_runtime.as_ref().and_then(|runtime| {
            runtime.is_busy().then(|| {
                runtime
                    .prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::List)
                    .ok()
            })?
        });
        if let (Some(runtime), Some(snapshot)) = (queue_runtime, queue_snapshot) {
            if snapshot.running_item().is_some() && !snapshot.paused {
                // Persist the pause before waking a blocked interaction. The
                // interaction may be the last thing keeping the active task
                // alive; cancelling it first could let the next FIFO item
                // start before the interrupt command reaches the actor.
                let _ =
                    runtime.prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::Pause {
                        expected_revision: snapshot.revision,
                    });
            }
            if snapshot.running_item().is_some() {
                self.cancel_queue_interactions();
                runtime
                    .interrupt_active()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                return Ok(true);
            }
        }
        let mut hosted = self.lock_hosted();
        if hosted.shutdown || !hosted.surface_activation_armed {
            return Ok(false);
        }
        hosted.interrupt_requested = true;
        Ok(true)
    }

    pub(crate) fn request_background_current(&self) -> bool {
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            return false;
        }
        if let Some(surface) = hosted.surface_active.as_mut() {
            surface.background_requested = true;
        } else {
            hosted.background_requested = true;
        }
        drop(hosted);
        self.hosted.changed.notify_all();
        true
    }

    pub(crate) fn queue_prompt(
        &self,
        prompt: String,
        bindings: orca_runtime::mentions::MentionBindings,
        images: Vec<orca_core::conversation::ImageInput>,
    ) -> io::Result<Option<orca_runtime::prompt_queue::PromptQueueSnapshot>> {
        let surface = self.lock_hosted().surface_active.clone();
        let Some(surface) = surface else {
            return Ok(None);
        };
        surface
            .client
            .prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::Add {
                input: orca_runtime::prompt_queue::PromptQueueInput {
                    text: prompt,
                    mention_bindings: bindings,
                    images,
                },
            })
            .map(Some)
            .map_err(|error| io::Error::other(format!("runtime prompt queue failed: {error:?}")))
    }

    pub(crate) fn prompt_queue_action(
        &self,
        action: orca_runtime::prompt_queue::PromptQueueAction,
    ) -> io::Result<Option<orca_runtime::prompt_queue::PromptQueueSnapshot>> {
        let surface = self.lock_hosted().surface_active.clone();
        let Some(surface) = surface else {
            return Ok(None);
        };
        surface
            .client
            .prompt_queue(action)
            .map(Some)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    pub(crate) fn pause_current_goal(&self) -> io::Result<bool> {
        let surface = self.lock_hosted().surface_active.clone();
        let Some(surface) = surface else {
            return Ok(false);
        };
        let Some(goal_fence) = surface.goal_fence else {
            return Ok(false);
        };
        match surface
            .client
            .pause_goal_operation(orca_runtime::surface::SurfaceRequestId::new(), goal_fence)
            .map_err(|error| io::Error::other(format!("typed Goal pause failed: {error:?}")))?
        {
            orca_runtime::surface::MutationReply::Committed { .. } => Ok(true),
            orca_runtime::surface::MutationReply::Deferred { mutation, .. } => {
                Err(io::Error::other(format!(
                    "typed Goal pause deferred: request={:?} commit={:?}",
                    mutation.request_id, mutation.commit_id
                )))
            }
            orca_runtime::surface::MutationReply::Uncommitted { mutation } => Err(
                io::Error::other(format!("typed Goal pause did not commit: {mutation:?}")),
            ),
        }
    }

    pub(crate) fn shutdown(&self) {
        let _transition = self
            .hosted
            .surface_presentation_transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let surface_presentation_tasks = {
            let mut hosted = self.lock_hosted();
            hosted.shutdown = true;
            if let Some(stop) = hosted.queue_watcher_stop.take() {
                stop.store(true, Ordering::Release);
            }
            hosted.surface_delivery_watermarks = HashMap::new();
            hosted.surface_terminal_deliveries = HashSet::new();
            for task in &hosted.surface_presentation_tasks {
                task.cancelled.store(true, Ordering::Release);
            }
            std::mem::take(&mut hosted.surface_presentation_tasks)
        };
        self.cancel_surface_and_notify();
        self.cancel_queue_interactions();
        for task in surface_presentation_tasks {
            let _ = task.handle.join();
        }
    }

    pub(crate) fn begin_surface_activation(&self) -> io::Result<bool> {
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TUI surface task control is shutting down",
            ));
        }
        if hosted.surface_activation_armed {
            return Ok(false);
        }
        if hosted.surface_active.is_some() {
            return Err(io::Error::other("TUI operation is still active"));
        }
        hosted.surface_activation_armed = true;
        hosted.interrupt_requested = false;
        drop(hosted);
        self.hosted.changed.notify_all();
        Ok(true)
    }

    pub(crate) fn cancel_surface_activation(&self) {
        let mut hosted = self.lock_hosted();
        hosted.surface_activation_armed = false;
        hosted.interrupt_requested = false;
        drop(hosted);
        self.hosted.changed.notify_all();
    }

    /// Drop a pre-activation arm left by an input that was redirected to the
    /// runtime queue. A real typed operation owns `surface_active` and must
    /// never have its activation state cleared by this recovery path.
    pub(crate) fn cancel_surface_activation_if_idle(&self) {
        let mut hosted = self.lock_hosted();
        if hosted.surface_active.is_none() {
            hosted.surface_activation_armed = false;
            hosted.interrupt_requested = false;
        }
        drop(hosted);
        self.hosted.changed.notify_all();
    }

    pub(crate) fn bind_prompt_queue_runtime(
        &self,
        runtime: orca_runtime::runtime_host::RuntimeThreadHandle,
        event_tx: &Sender<TuiEvent>,
    ) -> Option<Arc<AtomicBool>> {
        let mut hosted = self.lock_hosted();
        let watcher_needs_restart = hosted
            .queue_runtime
            .as_ref()
            .is_none_or(|current| current.thread_id() != runtime.thread_id())
            || hosted
                .queue_watcher_stop
                .as_ref()
                .is_none_or(|stop| stop.load(Ordering::Acquire));
        if watcher_needs_restart {
            if let Some(stop) = hosted.queue_watcher_stop.take() {
                stop.store(true, Ordering::Release);
            }
            hosted.queue_watcher_stop = Some(Arc::new(AtomicBool::new(false)));
        }
        hosted.queue_runtime = Some(runtime);
        *hosted
            .queue_event_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event_tx.clone());
        watcher_needs_restart.then(|| {
            hosted
                .queue_watcher_stop
                .as_ref()
                .expect("queue watcher stop token")
                .clone()
        })
    }

    #[cfg(test)]
    pub(crate) fn bind_prompt_queue_event_sender(&self, event_tx: &Sender<TuiEvent>) {
        let hosted = self.lock_hosted();
        *hosted
            .queue_event_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event_tx.clone());
    }

    pub(crate) fn prompt_queue_event_sender(&self) -> Arc<Mutex<Option<Sender<TuiEvent>>>> {
        self.lock_hosted().queue_event_tx.clone()
    }

    pub(crate) fn await_queue_interaction(
        &self,
        request_id: String,
        kind: TuiInteractionKind,
        event: impl FnOnce(TuiInteractionKey) -> TuiEvent,
    ) -> io::Result<TuiInteractionResponse> {
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        let (key, event_tx) = {
            let mut hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI surface task control is shutting down",
                ));
            }
            let key = TuiInteractionKey::new(self.surface_ids.allocate(), request_id, kind);
            let event_tx = hosted
                .queue_event_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "queued runtime interaction channel is not installed",
                    )
                })?;
            hosted.queue_interactions.insert(key.clone(), response_tx);
            (key, event_tx)
        };
        if event_tx.send(event(key.clone())).is_err() {
            self.lock_hosted().queue_interactions.remove(&key);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while requesting queued interaction",
            ));
        }
        let response = response_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Interrupted,
                "queued runtime interaction response channel closed",
            )
        })?;
        self.lock_hosted().queue_interactions.remove(&key);
        response
    }

    pub(crate) fn respond_queue_interaction(
        &self,
        key: &TuiInteractionKey,
        response: &TuiInteractionResponse,
    ) -> io::Result<bool> {
        let Some(sender) = self.lock_hosted().queue_interactions.remove(key) else {
            return Ok(false);
        };
        sender.send(Ok(response.clone())).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "queued runtime interaction waiter closed",
            )
        })?;
        Ok(true)
    }

    fn cancel_queue_interactions(&self) {
        let waiters = {
            let mut hosted = self.lock_hosted();
            hosted
                .queue_interactions
                .drain()
                .map(|(_, sender)| sender)
                .collect::<Vec<_>>()
        };
        for sender in waiters {
            let _ = sender.send(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "queued runtime interaction cancelled",
            )));
        }
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.lock_hosted().shutdown
    }

    pub(crate) fn spawn_surface_presentation(
        &self,
        operation_id: orca_runtime::surface::SurfaceOperationId,
        name: &str,
        task: impl FnOnce(SurfacePresentationCancellation) + Send + 'static,
    ) -> io::Result<()> {
        let _transition = self
            .hosted
            .surface_presentation_transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let completed = {
            let mut hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI surface task control is shutting down",
                ));
            }
            let mut completed = Vec::new();
            let mut running = Vec::with_capacity(hosted.surface_presentation_tasks.len());
            for presentation in hosted.surface_presentation_tasks.drain(..) {
                if presentation.operation_id == operation_id || presentation.handle.is_finished() {
                    presentation.cancelled.store(true, Ordering::Release);
                    completed.push(presentation.handle);
                } else {
                    running.push(presentation);
                }
            }
            hosted.surface_presentation_tasks = running;
            completed
        };
        for task in completed {
            let _ = task.join();
        }
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TUI surface task control is shutting down",
            ));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = SurfacePresentationCancellation {
            cancelled: Arc::clone(&cancelled),
        };
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || task(cancellation))?;
        hosted
            .surface_presentation_tasks
            .push(SurfacePresentationTask {
                operation_id,
                cancelled,
                handle,
            });
        Ok(())
    }

    pub(crate) fn retire_surface_presentation(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) {
        let _transition = self
            .hosted
            .surface_presentation_transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retired = {
            let mut hosted = self.lock_hosted();
            let mut retired = Vec::new();
            let mut running = Vec::with_capacity(hosted.surface_presentation_tasks.len());
            for presentation in hosted.surface_presentation_tasks.drain(..) {
                if &presentation.operation_id == operation_id {
                    presentation.cancelled.store(true, Ordering::Release);
                    retired.push(presentation.handle);
                } else {
                    running.push(presentation);
                }
            }
            hosted.surface_presentation_tasks = running;
            retired
        };
        for task in retired {
            let _ = task.join();
        }
    }

    pub(crate) fn begin_surface_background_handoff(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) -> bool {
        let mut hosted = self.lock_hosted();
        let Some(surface) = hosted
            .surface_active
            .as_mut()
            .filter(|surface| &surface.operation_id == operation_id)
        else {
            return false;
        };
        if surface.background_handoff_pending || !std::mem::take(&mut surface.background_requested)
        {
            return false;
        }
        surface.background_handoff_pending = true;
        true
    }

    pub(crate) fn commit_surface_background_handoff(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) -> bool {
        let mut hosted = self.lock_hosted();
        let committed = hosted.surface_active.as_ref().is_some_and(|surface| {
            &surface.operation_id == operation_id && surface.background_handoff_pending
        });
        if committed {
            hosted.surface_active = None;
            hosted.surface_activation_armed = false;
            hosted.interrupt_requested = false;
        }
        drop(hosted);
        self.hosted.changed.notify_all();
        committed
    }

    pub(crate) fn rollback_surface_background_handoff(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) {
        let cancel = {
            let mut hosted = self.lock_hosted();
            let should_cancel = hosted.shutdown || hosted.interrupt_requested;
            let Some(surface) = hosted
                .surface_active
                .as_mut()
                .filter(|surface| &surface.operation_id == operation_id)
            else {
                return;
            };
            if !surface.background_handoff_pending {
                return;
            }
            surface.background_handoff_pending = false;
            should_cancel.then(|| (surface.client.clone(), surface.operation_id.clone()))
        };
        if let Some((client, operation_id)) = cancel {
            let _ = client
                .cancel_operation(orca_runtime::surface::SurfaceRequestId::new(), operation_id);
        }
    }

    fn cancel_surface_and_notify(&self) {
        let _ = cancel_surface_if_active(&mut self.lock_hosted());
        self.hosted.changed.notify_all();
    }

    pub(crate) fn install_surface(
        &self,
        client: orca_runtime::surface::RuntimeSurfaceClientHandle,
        operation_id: orca_runtime::surface::SurfaceOperationId,
    ) -> io::Result<()> {
        self.install_surface_with_goal(client, operation_id, None)
    }

    pub(crate) fn install_surface_goal(
        &self,
        client: orca_runtime::surface::RuntimeSurfaceClientHandle,
        operation_id: orca_runtime::surface::SurfaceOperationId,
        goal_fence: orca_runtime::surface::SurfaceGoalFence,
    ) -> io::Result<()> {
        self.install_surface_with_goal(client, operation_id, Some(goal_fence))
    }

    fn install_surface_with_goal(
        &self,
        client: orca_runtime::surface::RuntimeSurfaceClientHandle,
        operation_id: orca_runtime::surface::SurfaceOperationId,
        goal_fence: Option<orca_runtime::surface::SurfaceGoalFence>,
    ) -> io::Result<()> {
        self.retire_surface_presentation(&operation_id);
        let mut cancel_committed = false;
        let background_requested = loop {
            let mut hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI operation controller is shutting down",
                ));
            }
            if hosted.surface_active.is_some() {
                return Err(io::Error::other("TUI operation is still active"));
            }
            if hosted.interrupt_requested && !cancel_committed {
                drop(hosted);
                cancel_surface_operation_checked(&client, operation_id.clone())?;
                cancel_committed = true;
                continue;
            }
            let background_requested = hosted.background_requested;
            hosted.background_requested = false;
            hosted.surface_active = Some(SurfaceActiveOperation {
                client: client.clone(),
                operation_id: operation_id.clone(),
                goal_fence,
                ui_operation_id: self.surface_ids.allocate(),
                interactions: HashMap::new(),
                background_requested,
                background_handoff_pending: false,
            });
            hosted.surface_activation_armed = false;
            hosted.interrupt_requested = false;
            break background_requested;
        };
        self.hosted.changed.notify_all();
        if background_requested {
            self.hosted.changed.notify_all();
        }
        Ok(())
    }

    pub(crate) fn complete_surface(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) {
        let mut hosted = self.lock_hosted();
        if hosted
            .surface_active
            .as_ref()
            .is_some_and(|active| &active.operation_id == operation_id)
        {
            hosted.surface_active = None;
        }
        hosted.surface_activation_armed = false;
        hosted.interrupt_requested = false;
        drop(hosted);
        self.hosted.changed.notify_all();
    }

    pub(crate) fn remember_surface_delivery_watermark(
        &self,
        operation_id: orca_runtime::surface::SurfaceOperationId,
        watermark: TuiStreamDeliveryWatermark,
    ) {
        self.lock_hosted()
            .surface_delivery_watermarks
            .insert(operation_id, watermark);
    }

    pub(crate) fn surface_delivery_watermark(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) -> TuiStreamDeliveryWatermark {
        self.lock_hosted()
            .surface_delivery_watermarks
            .get(operation_id)
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn clear_surface_delivery_watermark(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) {
        self.lock_hosted()
            .surface_delivery_watermarks
            .remove(operation_id);
    }

    pub(crate) fn surface_terminal_was_delivered(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) -> bool {
        self.lock_hosted()
            .surface_terminal_deliveries
            .contains(operation_id)
    }

    pub(crate) fn remember_surface_terminal_delivery(
        &self,
        operation_id: orca_runtime::surface::SurfaceOperationId,
    ) {
        self.lock_hosted()
            .surface_terminal_deliveries
            .insert(operation_id);
    }

    pub(crate) fn register_surface_interaction(
        &self,
        interaction: &orca_runtime::surface::SurfaceInteractionView,
    ) -> Option<TuiEvent> {
        let mut hosted = self.lock_hosted();
        let active = hosted.surface_active.as_mut()?;
        if active.operation_id != interaction.fence.operation_id {
            return None;
        }
        let request_id = format!("{:?}", interaction.interaction_id);
        let (kind, event, permissions) = match &interaction.request {
            orca_runtime::surface::SurfaceInteractionRequest::ToolApproval {
                tool,
                description,
                preview,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::Approval,
                );
                (
                    TuiInteractionKind::Approval,
                    TuiEvent::ApprovalNeeded {
                        key,
                        tool: tool.name.as_str().to_string(),
                        target: tool.target.as_ref().map(|value| value.as_str().to_string()),
                        preview: preview
                            .as_ref()
                            .or(Some(description))
                            .map(|value| value.as_str().to_string()),
                    },
                    None,
                )
            }
            orca_runtime::surface::SurfaceInteractionRequest::PermissionRequest {
                tool_call_id,
                reason,
                permissions,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::Permission,
                );
                let tool = tool_call_id.as_str().to_string();
                (
                    TuiInteractionKind::Permission,
                    TuiEvent::PermissionApprovalNeeded {
                        key,
                        tool,
                        target: None,
                        preview: reason.as_ref().map(|value| value.as_str().to_string()),
                        permission_kind: permission_kind(permissions),
                    },
                    Some(permissions.clone()),
                )
            }
            orca_runtime::surface::SurfaceInteractionRequest::UserInput {
                question,
                suggestions,
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::UserInput,
                );
                (
                    TuiInteractionKind::UserInput,
                    TuiEvent::UserInputRequested {
                        key,
                        question: question.as_str().to_string(),
                        choices: suggestions
                            .iter()
                            .map(|value| value.as_str().to_string())
                            .collect(),
                    },
                    None,
                )
            }
            orca_runtime::surface::SurfaceInteractionRequest::McpElicitation {
                server_name,
                message,
                request,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::McpElicitation,
                );
                let (mode, url, requested_schema_json) = match request {
                    orca_runtime::surface::SurfaceMcpElicitationRequest::Form {
                        requested_schema,
                        ..
                    } => (
                        crate::types::TuiMcpElicitationMode::Form,
                        None,
                        requested_schema.as_ref().map(|value| {
                            serde_json::to_string(value)
                                .expect("surface MCP schema is serializable")
                        }),
                    ),
                    orca_runtime::surface::SurfaceMcpElicitationRequest::Url {
                        raw_url,
                        requested_schema,
                    } => (
                        crate::types::TuiMcpElicitationMode::Url,
                        raw_url.as_ref().map(|value| value.as_str().to_string()),
                        requested_schema.as_ref().map(|value| {
                            serde_json::to_string(value)
                                .expect("surface MCP schema is serializable")
                        }),
                    ),
                };
                (
                    TuiInteractionKind::McpElicitation,
                    TuiEvent::McpElicitationRequested {
                        key,
                        server_name: server_name.as_str().to_string(),
                        mode,
                        message: message.as_str().to_string(),
                        url,
                        requested_schema_json,
                    },
                    None,
                )
            }
            _ => return None,
        };
        let key = match &event {
            TuiEvent::ApprovalNeeded { key, .. }
            | TuiEvent::PermissionApprovalNeeded { key, .. }
            | TuiEvent::UserInputRequested { key, .. }
            | TuiEvent::McpElicitationRequested { key, .. } => key.clone(),
            _ => return None,
        };
        active
            .interactions
            .entry(key)
            .or_insert(SurfaceInteractionBinding {
                client: active.client.clone(),
                interaction_id: interaction.interaction_id.clone(),
                kind,
                permissions,
            });
        Some(event)
    }

    pub(crate) fn respond_surface_interaction(
        &self,
        key: &TuiInteractionKey,
        response: &TuiInteractionResponse,
    ) -> io::Result<bool> {
        let binding = {
            let hosted = self.lock_hosted();
            hosted
                .surface_active
                .as_ref()
                .and_then(|active| active.interactions.get(key).cloned())
        };
        let Some(binding) = binding else {
            return Ok(false);
        };
        let answer = match (binding.kind, response) {
            (TuiInteractionKind::Approval, TuiInteractionResponse::Approval(approved)) => {
                orca_runtime::surface::SurfaceClientInteractionAnswer::ToolApproval {
                    decision: if *approved {
                        orca_runtime::surface::SurfaceAllowDeny::Allow
                    } else {
                        orca_runtime::surface::SurfaceAllowDeny::Deny
                    },
                }
            }
            (TuiInteractionKind::Permission, TuiInteractionResponse::Permission(approved)) => {
                let permissions = binding
                    .permissions
                    .clone()
                    .ok_or_else(|| io::Error::other("typed TUI permission profile is missing"))?;
                let decision = if *approved {
                    orca_runtime::surface::SurfacePermissionClientDecision::Allow {
                        scope: orca_runtime::surface::PermissionGrantScope::Turn,
                        permissions,
                        strict_auto_review: false,
                    }
                } else {
                    orca_runtime::surface::SurfacePermissionClientDecision::Deny {
                        scope: orca_runtime::surface::PermissionGrantScope::Turn,
                        permissions,
                        strict_auto_review: false,
                    }
                };
                orca_runtime::surface::SurfaceClientInteractionAnswer::PermissionRequest {
                    decision,
                }
            }
            (TuiInteractionKind::UserInput, TuiInteractionResponse::UserInput(answer)) => {
                orca_runtime::surface::SurfaceClientInteractionAnswer::UserInput {
                    decision: orca_runtime::surface::SurfaceUserInputDecision::Answer(
                        orca_runtime::surface::DisplayText::new(answer.clone()),
                    ),
                }
            }
            (
                TuiInteractionKind::McpElicitation,
                TuiInteractionResponse::McpElicitation {
                    accepted,
                    content_json,
                },
            ) => {
                let decision = if *accepted {
                    let content = serde_json::from_str(content_json.as_deref().unwrap_or("{}"))
                        .map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid typed MCP elicitation content: {error}"),
                            )
                        })?;
                    orca_runtime::surface::SurfaceMcpElicitationDecision::Accept { content }
                } else {
                    orca_runtime::surface::SurfaceMcpElicitationDecision::Decline
                };
                orca_runtime::surface::SurfaceClientInteractionAnswer::McpElicitation { decision }
            }
            _ => return Ok(false),
        };
        match binding.client.respond_interaction_by_id(
            orca_runtime::surface::SurfaceRequestId::new(),
            binding.interaction_id,
            answer,
        ) {
            Ok(orca_runtime::surface::MutationReply::Committed { .. }) => {
                let mut hosted = self.lock_hosted();
                if let Some(active) = hosted.surface_active.as_mut() {
                    active.interactions.remove(key);
                }
                Ok(true)
            }
            Ok(orca_runtime::surface::MutationReply::Deferred { .. })
            | Ok(orca_runtime::surface::MutationReply::Uncommitted { .. }) => Err(
                io::Error::other("typed TUI interaction response was not committed"),
            ),
            Err(error) => Err(io::Error::other(format!(
                "typed TUI interaction response failed: {error:?}"
            ))),
        }
    }

    pub(crate) fn respond(
        &self,
        key: &TuiInteractionKey,
        response: &TuiInteractionResponse,
    ) -> io::Result<bool> {
        if self.respond_surface_interaction(key, response)? {
            return Ok(true);
        }
        self.respond_queue_interaction(key, response)
    }

    #[cfg(test)]
    pub(crate) fn has_surface_active(&self) -> bool {
        self.lock_hosted().surface_active.is_some()
    }

    fn lock_hosted(&self) -> MutexGuard<'_, HostedOperationInner> {
        self.hosted
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone)]
struct SurfaceActiveOperation {
    client: orca_runtime::surface::RuntimeSurfaceClientHandle,
    operation_id: orca_runtime::surface::SurfaceOperationId,
    goal_fence: Option<orca_runtime::surface::SurfaceGoalFence>,
    ui_operation_id: OperationId,
    interactions: HashMap<TuiInteractionKey, SurfaceInteractionBinding>,
    background_requested: bool,
    background_handoff_pending: bool,
}

#[derive(Clone)]
struct SurfaceInteractionBinding {
    client: orca_runtime::surface::RuntimeSurfaceClientHandle,
    interaction_id: orca_runtime::surface::SurfaceInteractionId,
    kind: TuiInteractionKind,
    permissions: Option<orca_runtime::surface::SurfacePermissionProfile>,
}

impl std::fmt::Debug for SurfaceActiveOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceActiveOperation")
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

fn cancel_surface_if_active(hosted: &mut HostedOperationInner) -> bool {
    cancel_surface_if_active_checked(hosted).unwrap_or(false)
}

fn cancel_surface_if_active_checked(hosted: &mut HostedOperationInner) -> io::Result<bool> {
    let Some(surface) = hosted.surface_active.as_ref() else {
        return Ok(false);
    };
    if surface.background_handoff_pending {
        hosted.interrupt_requested = true;
        return Ok(true);
    }
    let surface = surface.clone();
    cancel_surface_operation_checked(&surface.client, surface.operation_id)?;
    Ok(true)
}

fn cancel_surface_operation_checked(
    client: &orca_runtime::surface::RuntimeSurfaceClientHandle,
    operation_id: orca_runtime::surface::SurfaceOperationId,
) -> io::Result<()> {
    let request_id = orca_runtime::surface::SurfaceRequestId::new();
    match retry_transient_surface_unavailability(
        || client.cancel_operation(request_id.clone(), operation_id.clone()),
        Duration::from_secs(5),
    )
    .map_err(|error| io::Error::other(format!("typed surface cancel failed: {error:?}")))?
    {
        orca_runtime::surface::MutationReply::Committed { .. } => Ok(()),
        orca_runtime::surface::MutationReply::Deferred { mutation, .. } => {
            Err(io::Error::other(format!(
                "typed surface cancel deferred: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )))
        }
        orca_runtime::surface::MutationReply::Uncommitted { mutation } => Err(io::Error::other(
            format!("typed surface cancel did not commit: {mutation:?}"),
        )),
    }
}

fn retry_transient_surface_unavailability<T>(
    mut operation: impl FnMut() -> Result<T, orca_runtime::surface::SurfaceClientCommandError>,
    timeout: Duration,
) -> Result<T, orca_runtime::surface::SurfaceClientCommandError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match operation() {
            Err(orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable)
                if std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

fn permission_kind(
    profile: &orca_runtime::surface::SurfacePermissionProfile,
) -> orca_runtime::runtime_permission::RuntimePermissionRequestKind {
    if profile
        .network
        .as_ref()
        .is_some_and(|network| network.enabled == Some(true) || !network.domains.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock;
    }
    if profile
        .file_system
        .as_ref()
        .and_then(|filesystem| filesystem.write.as_ref())
        .is_some_and(|paths| !paths.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite;
    }
    profile
        .shell
        .as_ref()
        .and_then(|shell| shell.unsandboxed.then_some(()))
        .map(|_| {
            orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry
        })
        .unwrap_or(orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::TuiSurfaceTaskControl;
    use orca_runtime::surface::{ByteOffset, SurfaceOperationId, SurfaceStreamId};

    use crate::surface_projection::TuiStreamDeliveryWatermark;

    fn test_surface_operation_id(seed: u8) -> SurfaceOperationId {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        SurfaceOperationId::try_from_bytes(bytes).expect("surface operation id")
    }

    #[test]
    fn typed_surface_control_is_constructible_without_a_local_interaction_broker() {
        let control = TuiSurfaceTaskControl::new();

        assert!(!control.is_shutdown());
        control.shutdown();
        assert!(control.is_shutdown());
    }

    #[test]
    fn transient_surface_unavailability_retries_until_success() {
        let mut calls = 0;
        let result = super::retry_transient_surface_unavailability(
            || {
                calls += 1;
                if calls < 3 {
                    Err(orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable)
                } else {
                    Ok("committed")
                }
            },
            std::time::Duration::from_secs(1),
        );

        assert_eq!(result.unwrap(), "committed");
        assert_eq!(calls, 3);
    }

    #[test]
    fn permanent_surface_error_is_not_retried() {
        let mut calls = 0;
        let error = super::retry_transient_surface_unavailability(
            || {
                calls += 1;
                Err::<(), _>(orca_runtime::surface::SurfaceClientCommandError::Unauthorized)
            },
            std::time::Duration::from_secs(1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            orca_runtime::surface::SurfaceClientCommandError::Unauthorized
        ));
        assert_eq!(calls, 1);
    }

    #[test]
    fn transient_surface_unavailability_stops_at_deadline() {
        let mut calls = 0;
        let error = super::retry_transient_surface_unavailability(
            || {
                calls += 1;
                Err::<(), _>(orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable)
            },
            std::time::Duration::ZERO,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable
        ));
        assert_eq!(calls, 1);
    }

    #[test]
    fn shutdown_signals_and_joins_surface_presentation_tasks() {
        let surface_control = TuiSurfaceTaskControl::new();
        let monitor_control = surface_control.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let monitor_exited = Arc::clone(&exited);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let operation_id = test_surface_operation_id(7);
        surface_control
            .spawn_surface_presentation(
                operation_id.clone(),
                "test-surface-presentation",
                move |cancellation| {
                    started_tx.send(()).expect("signal monitor start");
                    while !cancellation.is_cancelled() && !monitor_control.is_shutdown() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    monitor_exited.store(true, Ordering::Release);
                },
            )
            .expect("spawn supervised presentation");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("presentation monitor started");

        surface_control.shutdown();

        assert!(exited.load(Ordering::Acquire));
        assert!(
            surface_control
                .spawn_surface_presentation(operation_id, "late-surface-presentation", |_| {},)
                .is_err(),
            "shutdown must reject unowned presentation tasks"
        );
    }

    #[test]
    fn replacement_surface_presentation_retires_the_same_operation_observer() {
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let operation_id = test_surface_operation_id(9);
        let first_exited = Arc::new(AtomicBool::new(false));
        let first_exited_task = Arc::clone(&first_exited);
        let (first_started_tx, first_started_rx) = std::sync::mpsc::sync_channel(1);
        let (first_cancelled_tx, first_cancelled_rx) = std::sync::mpsc::sync_channel(1);
        let (release_first_tx, release_first_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .spawn_surface_presentation(
                operation_id.clone(),
                "first-surface-presentation",
                move |cancellation| {
                    first_started_tx.send(()).expect("first task started");
                    while !cancellation.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    first_cancelled_tx
                        .send(())
                        .expect("first task observed cancellation");
                    release_first_rx.recv().expect("release first presentation");
                    first_exited_task.store(true, Ordering::Release);
                },
            )
            .expect("first presentation");
        first_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first presentation started");

        let (replacement_started_tx, replacement_started_rx) = std::sync::mpsc::sync_channel(1);
        let replacement_controller = controller.clone();
        let replacement_operation_id = operation_id.clone();
        let (replacement_result_tx, replacement_result_rx) = std::sync::mpsc::sync_channel(1);
        let replacement = std::thread::spawn(move || {
            let result = replacement_controller.spawn_surface_presentation(
                replacement_operation_id,
                "replacement-surface-presentation",
                move |cancellation| {
                    replacement_started_tx
                        .send(())
                        .expect("replacement task started");
                    while !cancellation.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                },
            );
            replacement_result_tx
                .send(result)
                .expect("replacement result");
        });

        first_cancelled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first observer cancellation");
        assert!(
            replacement_started_rx.try_recv().is_err(),
            "replacement must not start before the prior observer settles"
        );
        release_first_tx
            .send(())
            .expect("allow first observer to settle");
        replacement_result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("replacement result")
            .expect("replacement presentation");
        replacement.join().expect("replacement spawner");

        replacement_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("replacement presentation started");
        assert!(
            first_exited.load(Ordering::Acquire),
            "replacement must join the previous observer before returning"
        );
        controller.retire_surface_presentation(&operation_id);
    }

    #[test]
    fn shutdown_waits_for_an_observer_already_being_retired() {
        let controller = TuiSurfaceTaskControl::new();
        let operation_id = test_surface_operation_id(11);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        controller
            .spawn_surface_presentation(
                operation_id.clone(),
                "retiring-surface-presentation",
                move |cancellation| {
                    started_tx.send(()).expect("observer started");
                    while !cancellation.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    cancelled_tx.send(()).expect("observer saw cancellation");
                    release_rx.recv().expect("release retiring observer");
                },
            )
            .expect("spawn observer");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observer started");

        let retiring_controller = controller.clone();
        let retiring_operation_id = operation_id.clone();
        let retire = std::thread::spawn(move || {
            retiring_controller.retire_surface_presentation(&retiring_operation_id);
        });
        cancelled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observer entered retirement");

        let (shutdown_done_tx, shutdown_done_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown_controller = controller.clone();
        let shutdown = std::thread::spawn(move || {
            shutdown_controller.shutdown();
            shutdown_done_tx.send(()).expect("shutdown result");
        });
        assert!(
            shutdown_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "shutdown must wait for the transition owner to join the retiring observer"
        );

        release_tx.send(()).expect("settle retiring observer");
        retire.join().expect("retire observer");
        shutdown_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown completed after retirement");
        shutdown.join().expect("shutdown thread");
    }

    #[test]
    fn queued_interaction_round_trip_wakes_runtime_waiter() {
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        controller.bind_prompt_queue_event_sender(&event_tx);
        let waiting = controller.clone();
        let waiter = std::thread::spawn(move || {
            waiting.await_queue_interaction(
                "request-1".to_string(),
                crate::types::TuiInteractionKind::UserInput,
                |key| crate::types::TuiEvent::UserInputRequested {
                    key,
                    question: "question".to_string(),
                    choices: Vec::new(),
                },
            )
        });
        let key = match event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("queued event")
        {
            crate::types::TuiEvent::UserInputRequested { key, .. } => key,
            event => panic!("unexpected queued event: {event:?}"),
        };
        assert!(
            controller
                .respond(
                    &key,
                    &crate::types::TuiInteractionResponse::UserInput("answer".to_string())
                )
                .expect("respond queued interaction")
        );
        assert_eq!(
            waiter
                .join()
                .expect("waiter join")
                .expect("waiter response"),
            crate::types::TuiInteractionResponse::UserInput("answer".to_string())
        );
    }

    #[test]
    fn surface_delivery_watermark_survives_background_detach_until_terminal_clear() {
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let mut operation_bytes = [7; 16];
        operation_bytes[6] = 0x77;
        operation_bytes[8] = 0x87;
        let operation_id =
            SurfaceOperationId::try_from_bytes(operation_bytes).expect("operation id");
        let mut stream_bytes = [8; 16];
        stream_bytes[6] = 0x78;
        stream_bytes[8] = 0x88;
        let stream_id = SurfaceStreamId::try_from_bytes(stream_bytes).expect("stream id");
        let watermark =
            TuiStreamDeliveryWatermark::from([(stream_id.clone(), ByteOffset::new(17))]);

        controller.remember_surface_delivery_watermark(operation_id.clone(), watermark.clone());
        assert_eq!(
            controller.surface_delivery_watermark(&operation_id),
            watermark
        );

        controller.clear_surface_delivery_watermark(&operation_id);
        assert!(
            controller
                .surface_delivery_watermark(&operation_id)
                .is_empty()
        );
    }

    #[test]
    fn queued_submission_releases_idle_surface_activation_arm() {
        let control = TuiSurfaceTaskControl::isolated_for_test();
        assert!(control.begin_surface_activation().expect("arm activation"));

        control.cancel_surface_activation_if_idle();

        assert!(
            control
                .begin_surface_activation()
                .expect("re-arm activation")
        );
    }
}
