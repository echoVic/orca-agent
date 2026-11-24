//! A subagent kept in its parent's process never asks anyone: what would
//! need an answer is refused, and the child reports it.

use std::io;

use orca_core::approval_types::{ApprovalRequest, ApprovalResolution};
use orca_core::tool_types::ToolRequest;

use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::runtime_approval::RuntimeApprovalHandler;
use crate::runtime_permission::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
};

pub(crate) const APPROVAL_REFUSED: &str = "This call needs an approval, and a subagent cannot ask for one. \
    Do not retry it. Go on with what you can do, and say in your reply what you could not do.";

pub(crate) const UNATTENDED_CHILD_CONTEXT: &str = " Your permissions were fixed when you were started. \
    A call that needs an approval is refused. Do not retry it; say in your reply what you could not do.";

/// Marks an approval that was refused because nobody may be asked, as
/// distinct from a prompt that failed.
#[derive(Debug)]
pub(crate) struct RefusedWithoutAsking;

impl std::fmt::Display for RefusedWithoutAsking {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(APPROVAL_REFUSED)
    }
}

impl std::error::Error for RefusedWithoutAsking {}

pub(crate) fn refused_without_asking(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<RefusedWithoutAsking>())
}

/// Answers every approval a child's tool call would ask for with a refusal.
pub(crate) struct RefusingApprovalHandler;

impl RuntimeApprovalHandler for RefusingApprovalHandler {
    fn resolve_interactive(
        &self,
        _approval: &ApprovalRequest,
        _request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            RefusedWithoutAsking,
        ))
    }
}

/// Answers every permission a child's tool call would ask for as a person's
/// "deny" does: nothing is granted, for this turn only.
pub(crate) struct RefusingPermissionHandler;

impl RuntimePermissionRequestHandler for RefusingPermissionHandler {
    fn request_permissions(
        &self,
        _request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        Ok(RuntimePermissionResponse {
            decision: PermissionResponseDecision::Deny,
            scope: PermissionGrantScope::Turn,
            permissions: RequestPermissionProfile::default(),
            strict_auto_review: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{
        APPROVAL_REFUSED, RefusedWithoutAsking, RefusingPermissionHandler, refused_without_asking,
    };
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile,
    };
    use crate::runtime_permission::{
        RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
        TurnPermissionOverlay,
    };
    use crate::surface::SurfacePermissionOrigin;

    #[test]
    fn a_refusal_is_told_from_a_prompt_that_failed() {
        let refusal = io::Error::new(io::ErrorKind::PermissionDenied, RefusedWithoutAsking);
        assert!(refused_without_asking(&refusal));
        assert_eq!(refusal.to_string(), APPROVAL_REFUSED);

        let failed = io::Error::from_raw_os_error(35);
        assert!(!refused_without_asking(&failed));
        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "no access");
        assert!(!refused_without_asking(&denied));
    }

    #[test]
    fn a_permission_request_is_denied_with_nothing_granted() {
        let request = RuntimePermissionRequest {
            id: "child-permission".to_string(),
            reason: None,
            permissions: RequestPermissionProfile::default(),
            context: RuntimePermissionContext::foreground(SurfacePermissionOrigin::CommandExec),
        };
        let handler = RefusingPermissionHandler;

        let responses = [
            handler
                .request_permissions(&request)
                .expect("a refusal is an answer"),
            handler
                .request_permissions_pre_side_effect(&request, &TurnPermissionOverlay::default())
                .expect("a refusal before a side effect is an answer"),
        ];

        for response in responses {
            assert_eq!(response.decision, PermissionResponseDecision::Deny);
            assert_eq!(response.scope, PermissionGrantScope::Turn);
            assert_eq!(response.permissions, RequestPermissionProfile::default());
            assert!(response.permissions.file_system.is_none());
            assert!(response.permissions.network.is_none());
            assert!(!response.strict_auto_review);
        }
    }
}
