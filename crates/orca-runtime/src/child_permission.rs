//! Permission bridge used by synchronous child agent loops.
//!
//! A child tool must never be allowed to reuse the foreground permission
//! context supplied by a model or by a parent tool.  This module owns the
//! child identity and rewrites every request at the last boundary before it
//! reaches the runtime permission handler.

use std::io;
use std::sync::{Arc, atomic::AtomicU64};
use std::thread;
use std::time::{Duration, Instant};

use crate::runtime_permission::{
    RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimePermissionResponse, TurnPermissionOverlay,
};
use crate::runtime_surface::{
    SubagentRevision, SurfaceSubagentId, SurfaceTaskId, SurfaceToolCallId, SurfaceTurnId,
    TaskRevision,
};
use crate::tasks::{DetachedSubagentBinding, TaskRegistry, detached_permission_interaction_id};
use orca_core::cancel::CancelToken;

/// The immutable owner of one child attempt. The activity source cursor and
/// the task's optimistic-concurrency revision are different clocks: the
/// former advances for child events, while the latter also advances when a
/// permission interaction is opened or resolved. Keep the admitted task
/// revision as a floor and read only the activity cursor per request.
#[derive(Clone)]
pub(crate) struct ChildPermissionIdentity {
    pub(crate) task_id: Option<SurfaceTaskId>,
    pub(crate) task_revision_floor: Option<TaskRevision>,
    pub(crate) agent_id: Option<SurfaceSubagentId>,
    pub(crate) turn_id: Option<SurfaceTurnId>,
    pub(crate) revision_source: Option<Arc<AtomicU64>>,
}

impl ChildPermissionIdentity {
    pub(crate) fn new(
        task_id: SurfaceTaskId,
        agent_id: SurfaceSubagentId,
        turn_id: SurfaceTurnId,
        revision_source: Arc<AtomicU64>,
    ) -> Self {
        Self::new_with_task_revision(
            task_id,
            TaskRevision::try_new(1).expect("one is a valid task revision"),
            agent_id,
            turn_id,
            revision_source,
        )
    }

    pub(crate) fn new_with_task_revision(
        task_id: SurfaceTaskId,
        task_revision: TaskRevision,
        agent_id: SurfaceSubagentId,
        turn_id: SurfaceTurnId,
        revision_source: Arc<AtomicU64>,
    ) -> Self {
        Self {
            task_id: Some(task_id),
            task_revision_floor: Some(task_revision),
            agent_id: Some(agent_id),
            turn_id: Some(turn_id),
            revision_source: Some(revision_source),
        }
    }

    fn context_for(&self, request_id: &str) -> io::Result<RuntimePermissionContext> {
        let task_id = self.task_id.clone().ok_or_else(missing_identity)?;
        let task_revision = self.task_revision_floor.ok_or_else(missing_identity)?;
        let agent_id = self.agent_id.clone().ok_or_else(missing_identity)?;
        let turn_id = self.turn_id.clone().ok_or_else(missing_identity)?;
        let revision_source = self.revision_source.as_ref().ok_or_else(missing_identity)?;
        if request_id.is_empty() {
            return Err(missing_identity());
        }
        let revision = revision_source.load(std::sync::atomic::Ordering::Acquire);
        let agent_revision = SubagentRevision::try_new(revision).map_err(|_| missing_identity())?;
        let tool_call_id =
            SurfaceToolCallId::try_new(request_id.to_owned()).map_err(|_| missing_identity())?;
        let activity_id = crate::runtime_surface::SurfaceActivityId::try_new(request_id.to_owned())
            .map_err(|_| missing_identity())?;
        Ok(RuntimePermissionContext::child(
            task_id,
            task_revision,
            agent_id,
            agent_revision,
            activity_id,
            turn_id,
            tool_call_id,
        ))
    }
}

/// An owned handler that scopes all child requests to the exact child task,
/// subagent, turn, and tool-call identity.  The wrapped request's original
/// context is deliberately ignored because it is model-controlled input.
pub(crate) struct ChildPermissionHandler {
    parent: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    identity: ChildPermissionIdentity,
}

impl ChildPermissionHandler {
    pub(crate) fn new(
        parent: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
        identity: ChildPermissionIdentity,
    ) -> Self {
        Self { parent, identity }
    }

    fn scoped_request(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionRequest> {
        let mut scoped = request.clone();
        scoped.context = self.identity.context_for(&request.id)?;
        Ok(scoped)
    }
}

impl RuntimePermissionRequestHandler for ChildPermissionHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let scoped = self.scoped_request(request)?;
        self.parent.request_permissions(&scoped)
    }

    fn request_permissions_pre_side_effect(
        &self,
        request: &RuntimePermissionRequest,
        permission_overlay: &TurnPermissionOverlay,
    ) -> io::Result<RuntimePermissionResponse> {
        let scoped = self.scoped_request(request)?;
        self.parent
            .request_permissions_pre_side_effect(&scoped, permission_overlay)
    }
}

/// Process-independent permission bridge for an async child. The worker
/// writes a signed request to the session mailbox and waits for the actor to
/// commit a response. It never falls back to an implicit allow or to the
/// parent generation's foreground identity.
pub(crate) struct DetachedPermissionHandler {
    registry: TaskRegistry,
    binding: DetachedSubagentBinding,
    identity: ChildPermissionIdentity,
    cancel: CancelToken,
    wait_timeout: Duration,
}

impl DetachedPermissionHandler {
    pub(crate) fn new(
        registry: TaskRegistry,
        binding: DetachedSubagentBinding,
        identity: ChildPermissionIdentity,
        cancel: CancelToken,
    ) -> Self {
        Self {
            registry,
            binding,
            identity,
            cancel,
            wait_timeout: Duration::from_secs(10 * 60),
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, wait_timeout: Duration) -> Self {
        self.wait_timeout = wait_timeout;
        self
    }

    fn request_inner(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let mut scoped = request.clone();
        scoped.context = self.identity.context_for(&request.id)?;
        let key = self
            .registry
            .enqueue_detached_permission_request(&self.binding, scoped.clone())
            .map_err(io::Error::other)?;
        let deadline = Instant::now() + self.wait_timeout;
        let terminal_error = |kind: io::ErrorKind, message: &str| {
            self.persist_terminal_deny(&key).err().map_or_else(
                || io::Error::new(kind, message),
                |error| {
                    io::Error::new(
                        kind,
                        format!("{message}; failed to persist terminal denial: {error}"),
                    )
                },
            )
        };
        loop {
            if self.cancel.is_cancelled() {
                return Err(terminal_error(
                    io::ErrorKind::Interrupted,
                    "detached permission request was cancelled",
                ));
            }
            if Instant::now() >= deadline {
                return Err(terminal_error(
                    io::ErrorKind::TimedOut,
                    "detached permission request timed out waiting for the runtime actor",
                ));
            }
            let Some(record) = self
                .registry
                .detached_permission_request(&key)
                .map_err(io::Error::other)?
            else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "detached permission request disappeared",
                ));
            };
            // The mailbox is child-writable.  Re-check the immutable owner
            // tuple and the original request before consuming any response;
            // validating a signature against a key supplied by the mailbox
            // alone would permit a forged request to install its own key.
            let owner_matches = matches!(
                &record.request.context,
                RuntimePermissionContext::Child {
                    task_id,
                    task_revision,
                    agent_id,
                    tool_call_id,
                    origin: crate::surface::SurfacePermissionOrigin::ChildAgent,
                    ..
                } if task_id.as_str() == self.binding.task_id
                    && *task_revision == self.binding.task_revision
                    && agent_id.as_str() == self.binding.subagent_id
                    && tool_call_id.as_str() == request.id
            );
            let expected_key = format!(
                "{}:{}:{}",
                self.binding.task_id,
                self.binding.attempt_id.as_str(),
                request.id
            );
            let expected_interaction_id =
                detached_permission_interaction_id(&self.binding, &request.id)
                    .map_err(io::Error::other)?;
            if !owner_matches
                || record.key != key
                || record.key != expected_key
                || record.interaction_id != expected_interaction_id
                || record.request != scoped
                || record.attempt_id != self.binding.attempt_id
                || record.authority_digest != self.binding.authority_digest
                || record.permission_response_public_key
                    != self.binding.permission_response_public_key
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "detached permission response owner does not match the admitted child",
                ));
            }
            if let Some(response) = record.response.clone() {
                // Cancellation/timeout may race with the mailbox read.  Treat
                // either condition as the linearization point before
                // consuming an actor response; otherwise a late Allow could
                // escape after the child has already been cancelled.
                if self.cancel.is_cancelled() {
                    return Err(terminal_error(
                        io::ErrorKind::Interrupted,
                        "detached permission request was cancelled",
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(terminal_error(
                        io::ErrorKind::TimedOut,
                        "detached permission request timed out waiting for the runtime actor",
                    ));
                }
                if record.permission_response_public_key
                    != self.binding.permission_response_public_key
                    || !record.verify_response_signature_with_key(
                        &self.binding.permission_response_public_key,
                    )
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "detached permission response failed actor signature verification",
                    ));
                }
                let response_digest = record.response_digest.clone().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "detached permission response is missing its digest",
                    )
                })?;
                self.registry
                    .acknowledge_detached_permission_request(
                        &key,
                        &record.request_digest,
                        &response_digest,
                    )
                    .map_err(io::Error::other)?;
                if self.cancel.is_cancelled() {
                    return Err(terminal_error(
                        io::ErrorKind::Interrupted,
                        "detached permission request was cancelled",
                    ));
                }
                return Ok(response);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Cancellation and timeout are terminal decisions at the process
    /// boundary. Persist a deny before returning so a restarted actor cannot
    /// rediscover the same request and prompt the user again. A response that
    /// won a race with cancellation is already terminal and is left intact.
    fn persist_terminal_deny(&self, key: &str) -> io::Result<()> {
        let Some(record) = self
            .registry
            .detached_permission_request(key)
            .map_err(io::Error::other)?
        else {
            // The actor may have acknowledged the response just before the
            // worker observed cancellation. There is no pending request left
            // to re-prompt, so this is already a terminal outcome.
            return Ok(());
        };
        let owner_matches = matches!(
            &record.request.context,
            RuntimePermissionContext::Child {
                task_id,
                agent_id,
                origin: crate::surface::SurfacePermissionOrigin::ChildAgent,
                ..
            } if task_id.as_str() == self.binding.task_id
                && agent_id.as_str() == self.binding.subagent_id
        );
        let expected_key = format!(
            "{}:{}:{}",
            self.binding.task_id,
            self.binding.attempt_id.as_str(),
            record.request.id
        );
        if !owner_matches
            || record.key != key
            || record.key != expected_key
            || record.attempt_id != self.binding.attempt_id
            || record.authority_digest != self.binding.authority_digest
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "detached permission terminal denial owner does not match the admitted child",
            ));
        }
        if let Some(response_digest) = record.response_digest.as_ref() {
            // Cancellation may race with an actor response. The child is
            // terminating and will not consume that response, so acknowledge
            // the exact durable record and remove the mailbox entry instead
            // of leaving a terminal tombstone behind.
            self.registry
                .acknowledge_detached_permission_request(
                    key,
                    &record.request_digest,
                    response_digest,
                )
                .map_err(io::Error::other)?;
            return Ok(());
        } else if record.response.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "detached permission response is missing its digest",
            ));
        }
        let response = RuntimePermissionResponse {
            decision: crate::protocol::PermissionResponseDecision::Deny,
            scope: crate::protocol::PermissionGrantScope::Turn,
            permissions: crate::protocol::RequestPermissionProfile::default(),
            strict_auto_review: false,
        };
        match self.registry.resolve_detached_permission_request(
            key,
            &record.request_digest,
            response,
        ) {
            Ok(()) => Ok(()),
            Err(error) => {
                // If the actor resolved the request concurrently, the child
                // is still terminating and must not leave an Allow response
                // available for a later retry. Acknowledge the exact raced
                // record (or treat an already-acknowledged record as done).
                let Some(latest) = self
                    .registry
                    .detached_permission_request(key)
                    .map_err(io::Error::other)?
                else {
                    return Ok(());
                };
                if latest.request_digest != record.request_digest {
                    return Err(io::Error::other(error));
                }
                let Some(response_digest) = latest.response_digest.as_ref() else {
                    return Err(io::Error::other(error));
                };
                if latest.response.is_none() {
                    return Err(io::Error::other(error));
                }
                self.registry
                    .acknowledge_detached_permission_request(
                        key,
                        &latest.request_digest,
                        response_digest,
                    )
                    .map_err(io::Error::other)?;
                Ok(())
            }
        }
    }
}

impl RuntimePermissionRequestHandler for DetachedPermissionHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        self.request_inner(request)
    }

    fn request_permissions_pre_side_effect(
        &self,
        request: &RuntimePermissionRequest,
        _permission_overlay: &TurnPermissionOverlay,
    ) -> io::Result<RuntimePermissionResponse> {
        self.request_inner(request)
    }
}

fn missing_identity() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "child permission identity is incomplete; refusing permission request",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};

    use orca_core::config::PermissionProfileNetworkAccess;

    use super::*;
    use crate::agent_continuation::{
        AgentPromptId, ChildAgentCoordinator, ContinuationCompatibility, CreateContinuationInput,
    };
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile,
    };
    use crate::runtime_permission::RuntimePermissionContext;
    use crate::runtime_surface::SurfacePermissionOrigin;
    use crate::subagent::SubagentIsolation;

    struct RecordingHandler {
        requests: Mutex<Vec<RuntimePermissionRequest>>,
    }

    impl RuntimePermissionRequestHandler for RecordingHandler {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> io::Result<RuntimePermissionResponse> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: false,
            })
        }
    }

    fn request() -> RuntimePermissionRequest {
        RuntimePermissionRequest {
            id: "child-tool-7".to_string(),
            reason: Some("network access".to_string()),
            permissions: RequestPermissionProfile {
                file_system: None,
                network: Some(crate::protocol::RequestNetworkPermissions {
                    enabled: None,
                    domains: vec![(
                        "example.test".to_string(),
                        PermissionProfileNetworkAccess::Allow,
                    )]
                    .into_iter()
                    .collect(),
                }),
            },
            context: RuntimePermissionContext::foreground(SurfacePermissionOrigin::CommandExec),
        }
    }

    #[test]
    fn child_handler_rewrites_foreground_context_to_exact_child_owner() {
        let parent = Arc::new(RecordingHandler {
            requests: Mutex::new(Vec::new()),
        });
        let revisions = Arc::new(AtomicU64::new(3));
        let identity = ChildPermissionIdentity::new(
            SurfaceTaskId::try_new("task-42").unwrap(),
            SurfaceSubagentId::try_new("agent-42").unwrap(),
            SurfaceTurnId::new(),
            revisions,
        );
        let turn_id = identity.turn_id.clone().unwrap();
        let handler = ChildPermissionHandler::new(parent.clone(), identity);

        handler.request_permissions(&request()).unwrap();
        let requests = parent.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let RuntimePermissionContext::Child {
            task_id,
            task_revision,
            agent_id,
            agent_revision,
            activity_id,
            turn_id: observed_turn,
            tool_call_id,
            origin,
        } = &requests[0].context
        else {
            panic!("child request was not scoped");
        };
        assert_eq!(task_id.as_str(), "task-42");
        assert_eq!(task_revision.get(), 1);
        assert_eq!(agent_id.as_str(), "agent-42");
        assert_eq!(agent_revision.get(), 3);
        assert_eq!(activity_id.as_str(), "child-tool-7");
        assert_eq!(observed_turn, &turn_id);
        assert_eq!(tool_call_id.as_str(), "child-tool-7");
        assert_eq!(*origin, SurfacePermissionOrigin::ChildAgent);
    }

    #[test]
    fn child_handler_denies_when_identity_is_incomplete_without_calling_parent() {
        let parent = Arc::new(RecordingHandler {
            requests: Mutex::new(Vec::new()),
        });
        let identity = ChildPermissionIdentity {
            task_id: None,
            task_revision_floor: Some(TaskRevision::try_new(1).unwrap()),
            agent_id: Some(SurfaceSubagentId::try_new("agent-42").unwrap()),
            turn_id: Some(SurfaceTurnId::new()),
            revision_source: Some(Arc::new(AtomicU64::new(1))),
        };
        let handler = ChildPermissionHandler::new(parent.clone(), identity);

        let error = handler.request_permissions(&request()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains("child permission identity is incomplete")
        );
        assert!(parent.requests.lock().unwrap().is_empty());
    }

    fn detached_handler_for_registry(registry: TaskRegistry) -> DetachedPermissionHandler {
        let task =
            registry.create_subagent("detached child".to_string(), Some("general".to_string()));
        let coordinator = ChildAgentCoordinator::with_owner_id(
            registry.clone(),
            "detached-permission-test-owner".to_string(),
        )
        .expect("continuation coordinator");
        let prepared = coordinator
            .create(CreateContinuationInput {
                continuation_id: None,
                parent_task_id: None,
                task_id: task.id.clone(),
                prompt_id: AgentPromptId::new(),
                compatibility: ContinuationCompatibility {
                    subagent_type: "general".to_string(),
                    model: Some("test-model".to_string()),
                    isolation: SubagentIsolation::None,
                    effective_cwd: std::env::temp_dir().display().to_string(),
                    worktree: None,
                    compatibility_hash: crate::runtime_surface::Sha256Digest::new([3; 32]),
                },
            })
            .expect("prepared continuation");
        let parent_fence = crate::runtime_surface::SurfaceOperationFence {
            thread_id: crate::runtime_surface::SurfaceThreadId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            thread_owner_epoch: crate::runtime_surface::ThreadOwnerEpoch::new(1),
            operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            generation_id: crate::runtime_surface::SurfaceGenerationId::new(1),
        };
        let binding = registry
            .register_detached_subagent_binding(
                &task.id,
                &task.id,
                prepared.attempt_id,
                TaskRevision::try_new(1).unwrap(),
                Some(parent_fence),
            )
            .expect("detached binding");
        let identity = ChildPermissionIdentity::new(
            SurfaceTaskId::try_new(task.id.clone()).unwrap(),
            SurfaceSubagentId::try_new(task.id.clone()).unwrap(),
            SurfaceTurnId::new(),
            Arc::new(AtomicU64::new(1)),
        );
        let handler =
            DetachedPermissionHandler::new(registry.clone(), binding, identity, CancelToken::new());
        handler
    }

    fn detached_handler_fixture() -> (TaskRegistry, DetachedPermissionHandler) {
        let registry = TaskRegistry::new("detached-permission-test".to_string());
        let handler = detached_handler_for_registry(registry.clone());
        (registry, handler)
    }

    fn persistent_detached_handler_fixture()
    -> (tempfile::TempDir, TaskRegistry, DetachedPermissionHandler) {
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new_persistent(
            "detached-permission-persistent-test".to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let handler = detached_handler_for_registry(registry.clone());
        (temp, registry, handler)
    }

    #[test]
    fn detached_cancel_persists_terminal_deny() {
        let (registry, handler) = detached_handler_fixture();
        handler.cancel.cancel();

        let error = handler.request_permissions(&request()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let requests = registry.detached_permission_requests().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .response
                .as_ref()
                .map(|response| response.decision),
            Some(PermissionResponseDecision::Deny)
        );
    }

    #[test]
    fn detached_permission_retry_reuses_the_original_interaction_identity() {
        let (registry, handler) = detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();

        let first_key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped.clone())
            .unwrap();
        let first = registry
            .detached_permission_request(&first_key)
            .unwrap()
            .unwrap();

        let retry_key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap();
        let retry = registry
            .detached_permission_request(&retry_key)
            .unwrap()
            .unwrap();

        assert_eq!(retry_key, first_key);
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert_eq!(retry.request_digest, first.request_digest);
        assert!(retry.response.is_none());
    }

    #[test]
    fn detached_permission_retry_rejects_conflicting_payload_with_the_same_request_id() {
        let (registry, handler) = detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();
        registry
            .enqueue_detached_permission_request(&handler.binding, scoped.clone())
            .unwrap();

        scoped.reason = Some("different capability request".to_string());
        let error = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap_err();
        assert!(error.contains("conflicting request"));
    }

    #[test]
    fn persistent_detached_permission_retry_reuses_the_original_interaction_identity() {
        let (_temp, registry, handler) = persistent_detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();

        let first_key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped.clone())
            .unwrap();
        let first = registry
            .detached_permission_request(&first_key)
            .unwrap()
            .unwrap();
        let retry_key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap();
        let retry = registry
            .detached_permission_request(&retry_key)
            .unwrap()
            .unwrap();

        assert_eq!(retry_key, first_key);
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert_eq!(retry.request_digest, first.request_digest);
    }

    #[test]
    fn persistent_detached_permission_concurrent_retries_share_identity() {
        let (_temp, registry, handler) = persistent_detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let registry = registry.clone();
            let binding = handler.binding.clone();
            let scoped = scoped.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                registry
                    .enqueue_detached_permission_request(&binding, scoped)
                    .expect("concurrent retry should be idempotent")
            }));
        }
        let keys = workers
            .into_iter()
            .map(|worker| worker.join().expect("retry worker"))
            .collect::<Vec<_>>();
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
        let record = registry
            .detached_permission_request(&keys[0])
            .unwrap()
            .expect("persisted mailbox request");
        assert!(record.response.is_none());
        assert!(record.verify_request_digest());
        assert_eq!(registry.detached_permission_requests().unwrap().len(), 1);
    }

    #[test]
    fn detached_allow_response_is_actor_signed_and_binds_to_response_content() {
        let (registry, handler) = detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();
        let key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap();
        let record = registry
            .detached_permission_request(&key)
            .unwrap()
            .expect("mailbox request");
        let response = RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: RequestPermissionProfile::default(),
            strict_auto_review: false,
        };

        registry
            .resolve_detached_permission_request(&key, &record.request_digest, response.clone())
            .unwrap();
        let resolved = registry
            .detached_permission_request(&key)
            .unwrap()
            .expect("resolved mailbox request");
        assert_eq!(resolved.response.as_ref(), Some(&response));
        assert_eq!(resolved.response_signature.as_ref().map(Vec::len), Some(64));
        assert!(
            resolved.verify_response_signature_with_key(
                &handler.binding.permission_response_public_key
            )
        );

        let mut forged = resolved.clone();
        forged
            .response
            .as_mut()
            .expect("response")
            .strict_auto_review = true;
        assert!(
            !forged.verify_response_signature_with_key(
                &handler.binding.permission_response_public_key
            )
        );
    }

    #[test]
    fn persistent_detached_allow_fails_closed_after_registry_restart_without_private_key() {
        let (temp, registry, handler) = persistent_detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();
        let key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap();
        let request_digest = registry
            .detached_permission_request(&key)
            .unwrap()
            .expect("mailbox request")
            .request_digest;
        drop(handler);
        drop(registry);

        let reopened = TaskRegistry::new_persistent(
            "detached-permission-persistent-test".to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let response = RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: RequestPermissionProfile::default(),
            strict_auto_review: false,
        };
        let error = reopened
            .resolve_detached_permission_request(&key, &request_digest, response)
            .unwrap_err();
        assert!(error.contains("signer is unavailable"));
        assert!(
            reopened
                .detached_permission_request(&key)
                .unwrap()
                .expect("pending request remains")
                .response
                .is_none()
        );
    }

    #[test]
    fn persistent_detached_timeout_denial_survives_registry_reload() {
        let (temp, registry, handler) = persistent_detached_handler_fixture();
        let handler = handler.with_timeout(Duration::ZERO);
        let error = handler.request_permissions(&request()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let key = registry
            .detached_permission_requests()
            .unwrap()
            .into_iter()
            .next()
            .expect("timeout leaves a terminal mailbox record")
            .key;
        drop(handler);
        drop(registry);

        let reopened = TaskRegistry::new_persistent(
            "detached-permission-persistent-test".to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let record = reopened
            .detached_permission_request(&key)
            .unwrap()
            .expect("terminal denial persisted across reload");
        assert_eq!(
            record.response.as_ref().map(|response| response.decision),
            Some(PermissionResponseDecision::Deny)
        );
    }

    #[test]
    fn detached_timeout_persists_terminal_deny() {
        let (registry, handler) = detached_handler_fixture();
        let handler = handler.with_timeout(Duration::ZERO);

        let error = handler.request_permissions(&request()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let requests = registry.detached_permission_requests().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .response
                .as_ref()
                .map(|response| response.decision),
            Some(PermissionResponseDecision::Deny)
        );
    }

    #[test]
    fn detached_cancel_acknowledges_a_response_that_won_the_race() {
        let (registry, handler) = detached_handler_fixture();
        let request = request();
        let mut scoped = request.clone();
        scoped.context = handler.identity.context_for(&request.id).unwrap();
        let key = registry
            .enqueue_detached_permission_request(&handler.binding, scoped)
            .unwrap();
        let response = RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: RequestPermissionProfile::default(),
            strict_auto_review: false,
        };
        registry
            .resolve_detached_permission_request(
                &key,
                &registry
                    .detached_permission_request(&key)
                    .unwrap()
                    .unwrap()
                    .request_digest,
                response,
            )
            .unwrap();
        handler.cancel.cancel();

        let error = handler.request_permissions(&request).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(registry.detached_permission_requests().unwrap().is_empty());
    }
}
