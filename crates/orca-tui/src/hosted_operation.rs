//! Hosted TUI operation-recovery transaction ownership.

use crossbeam_channel as mpsc;
use orca_runtime::runtime_host::RuntimeThreadHandle;
use orca_runtime::surface::SurfaceOperationId;

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) enum HostedOperationAction {
    Resume { operation_id: SurfaceOperationId },
    Cancel { operation_id: SurfaceOperationId },
}

pub(crate) fn handle_hosted_operation_action(
    action: HostedOperationAction,
    thread: Option<&RuntimeThreadHandle>,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(runtime_thread) = thread else {
        let _ = event_tx.send(TuiEvent::OperationRejected(
            "no recoverable operation is available".to_string(),
        ));
        return;
    };
    let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
    match action {
        HostedOperationAction::Resume { operation_id } => {
            if let Err(error) = actions.resume_operation(&operation_id, control, event_tx) {
                let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                    "failed to resume operation: {error}"
                )));
            }
        }
        HostedOperationAction::Cancel { operation_id } => {
            if let Err(error) = actions.cancel_operation(&operation_id, control, event_tx) {
                let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                    "failed to cancel operation: {error}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hosted_operation_recovery_shapes_exact_rejections() {
        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 3,
        ])
        .unwrap();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        handle_hosted_operation_action(
            HostedOperationAction::Resume {
                operation_id: operation_id.clone(),
            },
            None,
            &control,
            &event_tx,
        );
        handle_hosted_operation_action(
            HostedOperationAction::Cancel { operation_id },
            None,
            &control,
            &event_tx,
        );

        for _ in 0..2 {
            assert!(matches!(
                event_rx.try_recv(),
                Ok(TuiEvent::OperationRejected(message))
                    if message == "no recoverable operation is available"
            ));
        }
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn active_hosted_operation_recovery_preserves_typed_error_prefixes() {
        let _home = crate::test_support::isolate_orca_home();
        let mut config = crate::test_support::test_run_config();
        config.history_mode = orca_core::config::HistoryMode::Record;
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = runtime
            .handle()
            .start_thread(config, "operation recovery test")
            .expect("runtime thread");
        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 4,
        ])
        .unwrap();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        handle_hosted_operation_action(
            HostedOperationAction::Resume {
                operation_id: operation_id.clone(),
            },
            Some(&thread),
            &control,
            &event_tx,
        );
        handle_hosted_operation_action(
            HostedOperationAction::Cancel { operation_id },
            Some(&thread),
            &control,
            &event_tx,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "failed to resume operation: no recoverable operation is available"
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "failed to cancel operation: no recoverable operation is available"
        ));
        assert!(event_rx.try_recv().is_err());

        thread.shutdown().expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
