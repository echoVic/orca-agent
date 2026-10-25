use super::ComputerUseService;
use super::budget::FailureBudget;
use super::ownership::TurnWindows;
use crate::extension::ExtensionData;
use orca_core::cancel::CancelToken;
use std::sync::Arc;

/// What a turn keeps for computer use, in its extension store: the failure
/// budget and the first window snapshot.
pub(crate) struct TurnScope {
    pub(crate) budget: Arc<FailureBudget>,
    pub(crate) windows: Arc<TurnWindows>,
}

impl TurnScope {
    pub(crate) fn for_turn(turn_store: &ExtensionData) -> Self {
        Self {
            budget: FailureBudget::for_turn(turn_store),
            windows: TurnWindows::for_turn(turn_store),
        }
    }
}

/// Lives in the turn's extension store: created by the turn's first computer call,
/// dropped with the store when the turn ends.
pub(crate) struct TurnGuard {
    service: Arc<ComputerUseService>,
    windows: Arc<TurnWindows>,
}

impl TurnGuard {
    fn new(
        service: &Arc<ComputerUseService>,
        cancel: &CancelToken,
        windows: Arc<TurnWindows>,
    ) -> Self {
        service.start_turn(cancel);
        Self {
            service: Arc::clone(service),
            windows,
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.service.end_turn(self.windows.start().as_ref());
    }
}

pub(crate) fn begin_turn(
    service: &Arc<ComputerUseService>,
    turn_store: &ExtensionData,
    cancel: &CancelToken,
) {
    // Read first: `get_or_init` holds the store's lock while it builds the guard.
    let windows = TurnWindows::for_turn(turn_store);
    turn_store.get_or_init(|| TurnGuard::new(service, cancel, windows));
}
