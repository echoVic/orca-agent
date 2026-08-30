//! Permission bridge used by synchronous child agent loops.
//!
//! A child tool must never be allowed to reuse the foreground permission
//! context supplied by a model or by a parent tool.  This module owns the
//! child identity and rewrites every request at the last boundary before it
//! reaches the runtime permission handler.

use std::io;
use std::sync::{Arc, atomic::AtomicU64};

use crate::runtime_permission::{
    RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimePermissionResponse, TurnPermissionOverlay,
};
use crate::runtime_surface::{
    SubagentRevision, SurfaceSubagentId, SurfaceTaskId, SurfaceToolCallId, SurfaceTurnId,
    TaskRevision,
};

/// The immutable owner of one synchronous child attempt.  Activity and task
/// revisions are read from a shared source immediately before each request so
/// prompts cannot carry a stale revision after a child event is committed.
#[derive(Clone)]
pub(crate) struct ChildPermissionIdentity {
    pub(crate) task_id: Option<SurfaceTaskId>,
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
        Self {
            task_id: Some(task_id),
            agent_id: Some(agent_id),
            turn_id: Some(turn_id),
            revision_source: Some(revision_source),
        }
    }

    fn context_for(&self, request_id: &str) -> io::Result<RuntimePermissionContext> {
        let task_id = self.task_id.clone().ok_or_else(missing_identity)?;
        let agent_id = self.agent_id.clone().ok_or_else(missing_identity)?;
        let turn_id = self.turn_id.clone().ok_or_else(missing_identity)?;
        let revision_source = self.revision_source.as_ref().ok_or_else(missing_identity)?;
        if request_id.is_empty() {
            return Err(missing_identity());
        }
        let revision = revision_source.load(std::sync::atomic::Ordering::Acquire);
        let task_revision = TaskRevision::try_new(revision).map_err(|_| missing_identity())?;
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

fn missing_identity() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "child permission identity is incomplete; refusing permission request",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use orca_core::config::PermissionProfileNetworkAccess;

    use super::*;
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile,
    };
    use crate::runtime_permission::RuntimePermissionContext;
    use crate::runtime_surface::SurfacePermissionOrigin;

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
        assert_eq!(task_revision.get(), 3);
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
}
