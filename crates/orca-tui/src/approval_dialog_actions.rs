use crossbeam_channel as mpsc;

use crossterm::event::{KeyCode, KeyEvent};

use crate::approval_actions::resolve_approval_option;
use crate::protocol::UserAction;
use crate::shortcuts::{ApprovalShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::types::{AppState, ApprovalOption};

pub(crate) fn handle_approval_dialog_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if let KeyCode::Char(c) = key.code
        && let Some(option) = state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.option_for_key(c))
    {
        resolve_approval_option(state, action_tx, option);
        return;
    }

    match resolve_shortcut(ShortcutContext::Approval, *key) {
        Some(ShortcutAction::Approval(ApprovalShortcut::SelectAllow)) => {
            if let Some(dialog) = &mut state.approval_dialog {
                dialog.selected = dialog.selected.saturating_sub(1);
            }
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::SelectDeny)) => {
            if let Some(dialog) = &mut state.approval_dialog {
                let last = dialog.options.len().saturating_sub(1);
                dialog.selected = (dialog.selected + 1).min(last);
            }
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::ToggleSelection)) => {
            if let Some(dialog) = &mut state.approval_dialog {
                let len = dialog.options.len().max(1);
                dialog.selected = (dialog.selected + 1) % len;
            }
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::Confirm)) => {
            let option = state
                .approval_dialog
                .as_ref()
                .map(|dialog| dialog.current());
            if let Some(option) = option {
                resolve_approval_option(state, action_tx, option);
            }
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::Approve)) => {
            resolve_approval_option(state, action_tx, ApprovalOption::Once);
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::Deny)) => {
            resolve_approval_option(state, action_tx, ApprovalOption::Deny);
        }
        Some(_) | None => {}
    }
}
