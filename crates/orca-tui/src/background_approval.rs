use crossbeam_channel as mpsc;

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::TuiEvent;

pub(crate) fn submit_background_approval_response_for_tui(
    actions: Option<&TuiSurfaceActions>,
    approval_id: &str,
    approved: bool,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot resolve background approval before a session exists".to_string(),
        ));
        return;
    };

    match actions.resolve_background_approval(approval_id, approved, control, event_tx) {
        Ok((task_id, projection)) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let decision = if approved { "approved" } else { "denied" };
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Background approval {decision} for {task_id}."
            )));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
        }
    }
}
