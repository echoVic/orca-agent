use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::ReasoningEffort;

use crate::commands;
use crate::protocol::UserAction;
use crate::slash_command_actions::encode_settings_intent;
use crate::types::AppState;

const CONFIG_ROW_COUNT: usize = 3;

pub(crate) fn handle_config_dialog_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }

    match key.code {
        KeyCode::Esc => state.config_dialog = None,
        KeyCode::Up | KeyCode::BackTab => {
            if let Some(dialog) = state.config_dialog.as_mut() {
                dialog.selected = dialog
                    .selected
                    .checked_sub(1)
                    .unwrap_or(CONFIG_ROW_COUNT - 1);
            }
        }
        KeyCode::Down | KeyCode::Tab => {
            if let Some(dialog) = state.config_dialog.as_mut() {
                dialog.selected = (dialog.selected + 1) % CONFIG_ROW_COUNT;
            }
        }
        KeyCode::Left => cycle_selected(state, false),
        KeyCode::Right | KeyCode::Char(' ') => cycle_selected(state, true),
        KeyCode::Enter => apply_dialog(state, action_tx),
        _ => {}
    }
}

fn cycle_selected(state: &mut AppState, forward: bool) {
    let Some(dialog) = state.config_dialog.as_mut() else {
        return;
    };
    match dialog.selected {
        0 => {
            let models = commands::available_models();
            let current = models
                .iter()
                .position(|model| *model == dialog.model)
                .unwrap_or(0);
            dialog.model = models[cycle_index(current, models.len(), forward)].to_string();
        }
        1 => {
            let efforts = [
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ];
            let current = efforts
                .iter()
                .position(|effort| *effort == dialog.reasoning_effort)
                .unwrap_or(0);
            dialog.reasoning_effort = efforts[cycle_index(current, efforts.len(), forward)];
        }
        2 => {
            let modes = [
                ApprovalMode::Suggest,
                ApprovalMode::AutoEdit,
                ApprovalMode::FullAuto,
                ApprovalMode::Plan,
            ];
            let current = modes
                .iter()
                .position(|mode| *mode == dialog.approval_mode)
                .unwrap_or(0);
            dialog.approval_mode = modes[cycle_index(current, modes.len(), forward)];
        }
        _ => {}
    }
}

fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else {
        current.checked_sub(1).unwrap_or(len - 1)
    }
}

fn apply_dialog(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>) {
    let Some(dialog) = state.config_dialog.take() else {
        return;
    };
    if dialog.model == state.model_name
        && dialog.reasoning_effort == state.reasoning_effort
        && dialog.approval_mode == state.approval_mode
    {
        return;
    }
    let _ = action_tx.send(UserAction::SetModel(encode_settings_intent(
        Some(&dialog.model),
        Some(dialog.reasoning_effort),
        Some(dialog.approval_mode),
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    use crate::slash_command_actions::decode_settings_intent;
    use crate::types::ConfigDialog;

    fn state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        )
    }

    #[test]
    fn arrows_edit_each_runtime_setting_and_enter_dispatches_one_patch() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        state.config_dialog = Some(ConfigDialog {
            selected: 0,
            model: "auto".to_string(),
            reasoning_effort: ReasoningEffort::Max,
            approval_mode: ApprovalMode::Suggest,
        });

        for code in [
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Enter,
        ] {
            handle_config_dialog_key(
                &KeyEvent::new(code, KeyModifiers::NONE),
                &mut state,
                &action_tx,
            );
        }

        let UserAction::SetModel(intent) = action_rx.try_recv().expect("settings action") else {
            panic!("expected settings action");
        };
        let settings = decode_settings_intent(&intent).expect("settings intent");
        assert_eq!(settings.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(settings.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(settings.approval_mode, Some(ApprovalMode::AutoEdit));
        assert!(state.config_dialog.is_none());
    }

    #[test]
    fn escape_closes_without_dispatching() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        state.config_dialog = Some(ConfigDialog {
            selected: 0,
            model: state.model_name.clone(),
            reasoning_effort: state.reasoning_effort,
            approval_mode: state.approval_mode,
        });

        handle_config_dialog_key(
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );

        assert!(state.config_dialog.is_none());
        assert!(action_rx.try_recv().is_err());
    }
}
