pub(super) mod background;
pub(super) mod capability;
pub(super) mod commit;
pub(super) mod generation_context;
pub(super) mod goal;
pub(super) mod interaction;
pub(super) mod operation_recovery;

use capability::{CapabilityCommitEffect, CapabilityReply};

pub(crate) enum RuntimeActorEffect {
    CommitCapability(CapabilityCommitEffect),
    ReplyCapability(CapabilityReply),
    ReplyOperation {
        reply: std::sync::mpsc::SyncSender<
            Result<
                crate::runtime_surface::WaitOperationTerminalResult,
                crate::runtime_surface::SurfaceClientCommandError,
            >,
        >,
        result: Result<
            crate::runtime_surface::WaitOperationTerminalResult,
            crate::runtime_surface::SurfaceClientCommandError,
        >,
        nonblocking: bool,
    },
}
