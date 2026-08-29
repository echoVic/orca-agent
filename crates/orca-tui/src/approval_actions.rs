use crossbeam_channel as mpsc;

use crate::protocol::{TuiInteractionKind, TuiInteractionResponse, UserAction};
use crate::types::{AppState, AppStatus, ApprovalOption};

/// Resolve the approval dialog by the chosen option. The "always allow"
/// options record a session allowlist entry so later matching approvals are
/// auto-granted by the app event loop. The wire protocol stays a simple
/// allow/deny bool.
pub(crate) fn resolve_approval_option(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    option: ApprovalOption,
) {
    if let Some(dialog) = &state.approval_dialog {
        match option {
            ApprovalOption::AlwaysTool => {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_tool(&dialog.tool));
            }
            ApprovalOption::AlwaysTarget => {
                if let Some(target) = &dialog.target {
                    state
                        .approval_allowlist
                        .insert(AppState::approval_key_target(&dialog.tool, target));
                }
            }
            ApprovalOption::Once | ApprovalOption::Deny => {}
        }
    }
    resolve_approval(state, action_tx, option.is_approve());
}

fn resolve_approval(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>, approved: bool) {
    if state
        .approval_dialog
        .as_ref()
        .and_then(|dialog| dialog.background_task_id.as_ref())
        .is_some()
    {
        let Some(id) = state
            .approval_dialog
            .as_ref()
            .map(|dialog| dialog.id.clone())
        else {
            return;
        };
        let _ = action_tx.send(UserAction::ResolveBackgroundApproval { id, approved });
        state.set_status(AppStatus::Idle);
    } else {
        let Some(interaction) = state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.interaction.clone())
        else {
            return;
        };
        let response = match interaction.kind {
            TuiInteractionKind::Approval => TuiInteractionResponse::Approval(approved),
            TuiInteractionKind::Permission => TuiInteractionResponse::Permission(approved),
            TuiInteractionKind::UserInput | TuiInteractionKind::McpElicitation => return,
        };
        let _ = action_tx.send(UserAction::RespondToInteraction {
            key: interaction,
            response,
        });
        if approved {
            state.enter_running();
        } else {
            state.set_status(AppStatus::Idle);
        }
    }
    state.approval_dialog = None;
}
