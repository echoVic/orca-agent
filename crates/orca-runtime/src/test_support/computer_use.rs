//! Fake desktop injected below the real provider, approval broker and executor.

use super::*;
use orca_computer_use::fake::{FakeDesktop, FakeLauncher};

pub struct FakeComputerExecutor {
    pub launcher: FakeLauncher,
}

impl FakeComputerExecutor {
    pub fn new(desktop: FakeDesktop) -> Self {
        Self {
            launcher: FakeLauncher::new(desktop),
        }
    }
}

impl ThreadOperationExecutor for FakeComputerExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        thread.thread_extensions().get_or_init(|| {
            crate::computer_use::ComputerUseService::new_for_test(Arc::new(self.launcher.clone()))
        });
        LegacyThreadOperationExecutor.run_turn(thread, request, generation, events, writer, cancel)
    }
}

#[cfg(test)]
pub(crate) fn computer_approval_view(target: &str) -> surface::SurfaceInteractionView {
    use surface::*;
    let v7 = || *uuid::Uuid::now_v7().as_bytes();
    let fence = SurfaceOperationFence {
        thread_id: SurfaceThreadId::try_from_bytes(v7()).unwrap(),
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: SurfaceOperationId::try_from_bytes(v7()).unwrap(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let digest = Sha256Digest::new([1; 32]);
    let authority = AuthorityFingerprint::new(
        fence.operation_id.clone(),
        digest,
        digest,
        CanonicalPath::try_new(std::env::current_dir().unwrap()).unwrap(),
        digest,
        PolicyEpoch::try_new(1).unwrap(),
        digest,
        digest,
        digest,
    );
    SurfaceInteractionView {
        interaction_id: SurfaceInteractionId::try_from_bytes(v7()).unwrap(),
        revision: InteractionRevision::try_new(1).unwrap(),
        fence,
        kind: SurfaceInteractionKind::ToolApproval,
        request: SurfaceInteractionRequest::ToolApproval {
            tool: SurfaceToolRequest {
                tool_call_id: SurfaceToolCallId::try_new("computer-1").unwrap(),
                source_response_id: None,
                turn_id: SurfaceTurnId::new(),
                name: NonEmptyText::try_new("computer").unwrap(),
                action: SurfaceToolAction::Computer,
                target: Some(DisplayText::new(target)),
                raw_arguments: DisplayText::new(r#"{"action":"observe","app":"TextEdit"}"#),
                arguments_digest: digest,
            },
            description: DisplayText::new("computer requested computer"),
            preview: Some(DisplayText::new(format!("observe in {target}"))),
            authority,
        },
        route: SurfaceInteractionRoute::Unassigned {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
        },
        lifecycle: SurfaceInteractionLifecycle::Requested,
        recovery_disposition: InteractionUnavailableDisposition::FailOperation,
    }
}
