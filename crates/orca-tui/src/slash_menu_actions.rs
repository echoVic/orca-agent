use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{ReasoningEffort, RunConfig};

use crate::commands;
use crate::composer_textarea::{make_textarea, make_textarea_with_text, textarea_text};
use crate::protocol::UserAction;
use crate::slash_command_actions::encode_settings_intent;
use crate::slash_command_actions::{SlashOutcome, handle_slash_command, parse_approval_mode};
use crate::theme::Theme;
use crate::types::{AppState, SlashMenu, SlashMenuItem, SubMenu};
use crate::vim::VimState;

pub(crate) fn update_slash_menu(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    let text = textarea_text(textarea);
    if textarea.lines().len() == 1 && text.starts_with('/') {
        let filter = &text;
        let cwd = config
            .cwd
            .as_deref()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let items: Vec<SlashMenuItem> = commands::available_commands(&cwd)
            .into_iter()
            .filter(|(cmd, _)| cmd.starts_with(filter))
            .map(|(cmd, desc)| SlashMenuItem {
                command: cmd,
                description: desc,
            })
            .collect();
        if items.is_empty() {
            state.slash_menu = None;
        } else {
            let selected = state
                .slash_menu
                .as_ref()
                .map(|m| m.selected.min(items.len().saturating_sub(1)))
                .unwrap_or(0);
            state.slash_menu = Some(SlashMenu {
                items,
                selected,
                sub_menu: None,
            });
        }
    } else {
        state.slash_menu = None;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_slash_menu_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    let mut pending_settings = None;
    let menu = match &mut state.slash_menu {
        Some(m) => m,
        None => return false,
    };

    if let Some(sub) = &mut menu.sub_menu {
        match key.code {
            KeyCode::Up => {
                sub.selected = sub.selected.saturating_sub(1);
                return true;
            }
            KeyCode::Down => {
                if sub.selected + 1 < sub.items.len() {
                    sub.selected += 1;
                }
                return true;
            }
            KeyCode::Tab | KeyCode::Enter => {
                let chosen = sub.items[sub.selected].clone();
                let title = sub.title.clone();
                let pending_model = sub.context.clone();
                if title == "/model" {
                    let chosen_model = chosen
                        .split_whitespace()
                        .next()
                        .unwrap_or(&chosen)
                        .to_string();
                    if let Ok(()) = commands::validate_model(&chosen_model) {
                        menu.sub_menu = Some(reasoning_effort_submenu(
                            chosen_model,
                            state.reasoning_effort,
                        ));
                        return true;
                    }
                } else if title == REASONING_SUBMENU_TITLE {
                    if let (Some(model), Some(effort)) =
                        (pending_model, parse_reasoning_effort(&chosen))
                    {
                        pending_settings =
                            Some(encode_settings_intent(Some(&model), Some(effort), None));
                    }
                } else if title == "/mode"
                    && let Some(mode) = parse_approval_mode(&chosen)
                {
                    pending_settings = Some(encode_settings_intent(None, None, Some(mode)));
                }
                if let Some(settings) = pending_settings {
                    let _ = action_tx.send(UserAction::SetModel(settings));
                }
                state.slash_menu = None;
                *textarea = make_textarea(vim_state, theme);
                return true;
            }
            KeyCode::Esc => {
                state.slash_menu = None;
                *textarea = make_textarea(vim_state, theme);
                return true;
            }
            _ => return true,
        }
    }

    match key.code {
        KeyCode::Up => {
            menu.selected = menu.selected.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            if menu.selected + 1 < menu.items.len() {
                menu.selected += 1;
            }
            true
        }
        KeyCode::Tab => {
            let selected_cmd = menu.items[menu.selected].command.clone();
            if selected_cmd == "/goal" {
                *textarea = make_textarea_with_text("/goal ", vim_state, theme);
                state.slash_menu = None;
                return true;
            }
            // skill commands: fill textarea so user can add args before submitting
            let cwd = config
                .cwd
                .as_deref()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            if let Some(id) = selected_cmd.strip_prefix('/') {
                if let Ok(skills) = orca_tools::skills::discover_from_env(&cwd) {
                    if skills.iter().any(|s| s.id == id) {
                        *textarea =
                            make_textarea_with_text(&format!("{selected_cmd} "), vim_state, theme);
                        state.slash_menu = None;
                        return true;
                    }
                }
            }
            select_slash_menu_command(
                selected_cmd,
                menu.items.clone(),
                menu.selected,
                state,
                config,
                shared_config,
                action_tx,
                textarea,
                vim_state,
                theme,
            );
            true
        }
        KeyCode::Enter => {
            let selected_cmd = menu.items[menu.selected].command.clone();
            select_slash_menu_command(
                selected_cmd,
                menu.items.clone(),
                menu.selected,
                state,
                config,
                shared_config,
                action_tx,
                textarea,
                vim_state,
                theme,
            );
            true
        }
        KeyCode::Esc => {
            state.slash_menu = None;
            true
        }
        _ => {
            textarea.input(Input::from(ev.clone()));
            update_slash_menu(textarea, state, config);
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn select_slash_menu_command(
    selected_cmd: String,
    menu_items: Vec<SlashMenuItem>,
    selected: usize,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    match selected_cmd.as_str() {
        "/model" => {
            state.slash_menu = Some(SlashMenu {
                items: menu_items,
                selected,
                sub_menu: Some(model_submenu(&state.model_name)),
            });
        }
        "/mode" => {
            state.slash_menu = Some(SlashMenu {
                items: menu_items,
                selected,
                sub_menu: Some(approval_mode_submenu(state.approval_mode)),
            });
        }
        "/remember" => {
            *textarea = make_textarea_with_text("/remember ", vim_state, theme);
            state.slash_menu = None;
        }
        _ => {
            *textarea = make_textarea_with_text(&selected_cmd, vim_state, theme);
            state.slash_menu = None;
            if let Some(outcome) =
                handle_slash_command(&selected_cmd, config, shared_config, state, action_tx)
            {
                match outcome {
                    SlashOutcome::Continue => {
                        *textarea = make_textarea(vim_state, theme);
                    }
                    SlashOutcome::Prefill(value) => {
                        *textarea = make_textarea_with_text(&value, vim_state, theme);
                    }
                }
            }
        }
    }
}

pub(crate) const REASONING_SUBMENU_TITLE: &str = "/model · reasoning effort";

fn model_submenu(current: &str) -> SubMenu {
    let selected = commands::available_models()
        .iter()
        .position(|model| *model == current)
        .unwrap_or(0);
    let items = commands::available_models()
        .iter()
        .map(|model| match *model {
            "auto" => "auto (pro + flash for aux)".to_string(),
            other => other.to_string(),
        })
        .collect();
    SubMenu {
        title: "/model".to_string(),
        items,
        selected,
        context: None,
    }
}

fn approval_mode_submenu(current: ApprovalMode) -> SubMenu {
    let modes = [
        ApprovalMode::Suggest,
        ApprovalMode::AutoEdit,
        ApprovalMode::FullAuto,
        ApprovalMode::Plan,
    ];
    let selected = modes.iter().position(|mode| *mode == current).unwrap_or(0);
    SubMenu {
        title: "/mode".to_string(),
        items: modes
            .into_iter()
            .map(|mode| mode.as_str().to_string())
            .collect(),
        selected,
        context: None,
    }
}

fn reasoning_effort_submenu(pending_model: String, current: ReasoningEffort) -> SubMenu {
    let items: Vec<String> = reasoning_effort_options()
        .iter()
        .map(|(effort, description)| format!("{} {description}", effort.as_str()))
        .collect();
    let selected = reasoning_effort_options()
        .iter()
        .position(|(effort, _)| *effort == current)
        .unwrap_or(0);
    SubMenu {
        title: REASONING_SUBMENU_TITLE.to_string(),
        items,
        selected,
        context: Some(pending_model),
    }
}

fn reasoning_effort_options() -> &'static [(ReasoningEffort, &'static str)] {
    &[
        (ReasoningEffort::Low, "(fastest, light reasoning)"),
        (ReasoningEffort::High, "(balanced reasoning)"),
        (ReasoningEffort::Max, "(deepest reasoning, default)"),
    ]
}

fn parse_reasoning_effort(choice: &str) -> Option<ReasoningEffort> {
    let token = choice.split_whitespace().next().unwrap_or(choice);
    reasoning_effort_options()
        .iter()
        .find(|(effort, _)| effort.as_str() == token)
        .map(|(effort, _)| *effort)
}

#[cfg(test)]
mod tests {
    use super::{approval_mode_submenu, model_submenu};
    use orca_core::approval_types::ApprovalMode;

    #[test]
    fn settings_submenus_preselect_the_committed_values() {
        let model = model_submenu("deepseek-v4-pro");
        assert_eq!(model.items[model.selected], "deepseek-v4-pro");

        let mode = approval_mode_submenu(ApprovalMode::AutoEdit);
        assert_eq!(mode.items[mode.selected], "auto-edit");
    }
}
