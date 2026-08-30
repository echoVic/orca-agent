use crossbeam_channel as mpsc;
use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_runtime::onboarding::acknowledge_first_run_in;

use crate::composer_textarea::{make_setup_textarea, make_textarea};
use crate::protocol::UserAction;
use crate::surface_actions::TuiHostActions;
use crate::theme::Theme;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus};
use crate::vim::VimState;

pub(crate) enum SetupFlow {
    Continue,
    Exit(i32),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_setup_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
) -> io::Result<SetupFlow> {
    match state.setup_step {
        0 => match key.code {
            KeyCode::Enter => {
                let Some(first_run) = state.first_run.as_mut() else {
                    state.push_message(ChatMessage::Error(
                        state
                            .first_run_error
                            .clone()
                            .unwrap_or_else(|| "cannot inspect first-run security state".into()),
                    ));
                    return Ok(SetupFlow::Continue);
                };
                if !first_run.acknowledged {
                    if let Err(error) = acknowledge_first_run_in(first_run) {
                        state.push_message(ChatMessage::Error(format!(
                            "failed to record security disclosure: {error}"
                        )));
                        return Ok(SetupFlow::Continue);
                    }
                    first_run.acknowledged = true;
                }
                if config.api_key.is_none() {
                    state.setup_step = 1;
                    *textarea = make_setup_textarea(theme);
                } else {
                    state.setup_step = 2;
                }
            }
            KeyCode::Esc => {
                return Ok(SetupFlow::Exit(0));
            }
            _ => {}
        },
        1 => match key.code {
            KeyCode::Enter => {
                let lines: Vec<String> = textarea.lines().to_vec();
                let key_input = lines.join("").trim().to_string();
                if !key_input.is_empty() {
                    match TuiHostActions::save_api_key(&key_input) {
                        Ok(_) => {
                            config.api_key = Some(key_input.clone());
                            if let Ok(mut cfg) = shared_config.lock() {
                                cfg.api_key = Some(key_input);
                            }
                            state.setup_step = 2;
                        }
                        Err(error) => state.push_message(ChatMessage::Error(format!(
                            "failed to save API key: {error}"
                        ))),
                    }
                }
            }
            KeyCode::Esc => {
                return Ok(SetupFlow::Exit(0));
            }
            _ => {
                textarea.input(Input::from(ev.clone()));
            }
        },
        2 => match key.code {
            KeyCode::Enter => {
                state.set_status(AppStatus::Idle);
                state.setup_step = 0;
                *textarea = make_textarea(vim_state, theme);

                if let Some(prompt) = initial_prompt {
                    state.push_message(ChatMessage::User(prompt.clone()));
                    state.enter_running();
                    let _ = action_tx.send(UserAction::Submit(prompt));
                }
            }
            KeyCode::Esc => {
                return Ok(SetupFlow::Exit(0));
            }
            _ => {}
        },
        _ => {}
    }
    Ok(SetupFlow::Continue)
}
