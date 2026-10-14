use super::ComputerUseService;
use crate::extension::ExtensionData;
use orca_core::cancel::CancelToken;
use std::sync::Arc;

/// Lives in the turn's extension store: created by the turn's first computer call,
/// dropped with the store when the turn ends.
pub(crate) struct TurnGuard {
    service: Arc<ComputerUseService>,
}

impl TurnGuard {
    fn new(service: &Arc<ComputerUseService>, cancel: &CancelToken) -> Self {
        service.start_turn(cancel);
        Self {
            service: Arc::clone(service),
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.service.end_turn();
    }
}

pub(crate) fn begin_turn(
    service: &Arc<ComputerUseService>,
    turn_store: &ExtensionData,
    cancel: &CancelToken,
) {
    turn_store.get_or_init(|| TurnGuard::new(service, cancel));
}
