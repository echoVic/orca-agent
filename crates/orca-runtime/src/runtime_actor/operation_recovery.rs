use std::collections::HashMap;

use crate::runtime_surface as surface;

pub(crate) struct EphemeralReservationExpiry {
    pub(crate) operation_id: surface::SurfaceOperationId,
    pub(crate) expires_at: tokio::time::Instant,
}

pub(crate) struct OperationRecoveryController<ManualCompaction, ProviderTransfer> {
    pub(crate) pending_manual_compaction: Option<ManualCompaction>,
    pub(crate) pending_provider_transfer: Option<ProviderTransfer>,
    pub(crate) live_input_capsules:
        HashMap<surface::SurfaceOperationId, surface::SurfaceInputRequest>,
    pub(crate) ephemeral_reservation_expiry: Option<EphemeralReservationExpiry>,
    pub(crate) terminal_blocked: Option<String>,
}

impl<ManualCompaction, ProviderTransfer>
    OperationRecoveryController<ManualCompaction, ProviderTransfer>
{
    pub(crate) fn new() -> Self {
        Self {
            pending_manual_compaction: None,
            pending_provider_transfer: None,
            live_input_capsules: HashMap::new(),
            ephemeral_reservation_expiry: None,
            terminal_blocked: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OperationRecoveryController;

    #[test]
    fn recovery_state_has_one_owner_and_explicit_terminal_blocking() {
        let mut controller = OperationRecoveryController::<u8, u16>::new();
        assert!(controller.pending_manual_compaction.is_none());
        assert!(controller.pending_provider_transfer.is_none());
        controller.pending_manual_compaction = Some(1);
        assert!(controller.pending_manual_compaction.is_some());
        controller.pending_manual_compaction = None;
        controller.pending_provider_transfer = Some(2);
        assert!(controller.pending_provider_transfer.is_some());
        controller.terminal_blocked = Some("durable retry pending".to_string());
        assert_eq!(
            controller.terminal_blocked.as_deref(),
            Some("durable retry pending")
        );
        controller.terminal_blocked = None;
        assert!(controller.terminal_blocked.is_none());
    }
}
