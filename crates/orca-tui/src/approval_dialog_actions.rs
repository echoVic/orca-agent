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
        Some(ShortcutAction::Approval(ApprovalShortcut::PreviewPageUp)) => {
            let page = crate::ui::approval_preview_page(state);
            if let Some(dialog) = &mut state.approval_dialog {
                dialog.diff_scroll = dialog.diff_scroll.saturating_sub(page);
            }
        }
        Some(ShortcutAction::Approval(ApprovalShortcut::PreviewPageDown)) => {
            let page = crate::ui::approval_preview_page(state);
            if let Some(dialog) = &mut state.approval_dialog {
                let last_page = dialog
                    .diff
                    .as_deref()
                    .map_or(0, |diff| diff.lines().count().saturating_sub(page));
                dialog.diff_scroll = (dialog.diff_scroll + page).min(last_page);
            }
        }
        Some(_) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TuiInteractionKey, TuiInteractionKind};
    use crate::types::{AppStatus, ApprovalDialog};
    use crossterm::event::KeyModifiers;
    use orca_runtime::runtime_permission::RuntimePermissionRequestKind;

    /// A `WaitingApproval` state with an open dialog, plus the action
    /// channel `handle_approval_dialog_key` reports its resolution on. The
    /// dialog is permission-kind (rather than the plain tool-approval kind)
    /// so a denial resolves to `TuiPermissionDecision::Deny`, which shows up
    /// in the sent action's `Debug` output; a tool-approval denial would
    /// resolve to the indistinguishable `Approval(false)`. Both kinds
    /// travel through the same `resolve_approval` call either way, so the
    /// choice only affects what the test below can observe, not what code
    /// path Esc takes.
    fn approval_fixture() -> (
        AppState,
        mpsc::Sender<UserAction>,
        mpsc::Receiver<UserAction>,
    ) {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            event_tx,
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::WaitingApproval;
        state.approval_dialog = Some(ApprovalDialog {
            id: "approval-1".to_string(),
            interaction: Some(TuiInteractionKey::new(
                orca_core::cancel::OperationIdAllocator::new().allocate(),
                "approval-1",
                TuiInteractionKind::Permission,
            )),
            tool: "bash".to_string(),
            target: Some("curl https://api.example.invalid".to_string()),
            permission_kind: Some(RuntimePermissionRequestKind::NetworkBlock),
            background_task_id: None,
            selected: 0,
            options: ApprovalDialog::options_for("bash", Some("curl https://api.example.invalid")),
            diff: None,
            diff_scroll: 0,
        });
        let (action_tx, action_rx) = mpsc::unbounded();
        (state, action_tx, action_rx)
    }

    #[test]
    fn pressing_esc_resolves_the_approval_as_denied() {
        let (mut state, action_tx, action_rx) = approval_fixture();

        handle_approval_dialog_key(
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );

        assert!(state.approval_dialog.is_none(), "the dialog must close");
        let action = action_rx.try_recv().expect("a response is sent");
        assert!(format!("{action:?}").contains("Deny"), "{action:?}");
    }
}
