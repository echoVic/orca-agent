use crossbeam_channel as mpsc;
use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_core::config::folder_trust::{self, TrustLevel};
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

/// Selectable actions on the first-run welcome step, in navigation order.
pub(crate) const SETUP_TRUST_SELECTION: u8 = 0;
pub(crate) const SETUP_UNTRUSTED_SELECTION: u8 = 1;
pub(crate) const SETUP_EXIT_SELECTION: u8 = 2;

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
            KeyCode::Left | KeyCode::Up => {
                state.setup_selection = state.setup_selection.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Down => {
                state.setup_selection = (state.setup_selection + 1).min(SETUP_EXIT_SELECTION);
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                state.setup_selection = SETUP_TRUST_SELECTION;
                let Some(first_run) = state.first_run.as_mut() else {
                    return Ok(SetupFlow::Continue);
                };
                if let Err(error) = folder_trust::set_trust_with_config_dir(
                    &first_run.workspace,
                    &first_run.config_dir,
                    TrustLevel::Trusted,
                ) {
                    state.push_message(ChatMessage::Error(format!(
                        "failed to trust workspace: {error}"
                    )));
                } else {
                    first_run.workspace_trusted = true;
                }
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                state.setup_selection = SETUP_UNTRUSTED_SELECTION;
                let Some(first_run) = state.first_run.as_mut() else {
                    return Ok(SetupFlow::Continue);
                };
                if let Err(error) = folder_trust::set_trust_with_config_dir(
                    &first_run.workspace,
                    &first_run.config_dir,
                    TrustLevel::Untrusted,
                ) {
                    state.push_message(ChatMessage::Error(format!(
                        "failed to keep workspace untrusted: {error}"
                    )));
                } else {
                    first_run.workspace_trusted = false;
                }
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                state.setup_selection = SETUP_EXIT_SELECTION;
                return Ok(SetupFlow::Exit(0));
            }
            KeyCode::Enter => {
                if state.setup_selection == SETUP_EXIT_SELECTION {
                    return Ok(SetupFlow::Exit(0));
                }
                if matches!(
                    state.setup_selection,
                    SETUP_TRUST_SELECTION | SETUP_UNTRUSTED_SELECTION
                ) {
                    let trusted = state.setup_selection == SETUP_TRUST_SELECTION;
                    let Some(first_run) = state.first_run.as_mut() else {
                        return Ok(SetupFlow::Continue);
                    };
                    if let Err(error) = folder_trust::set_trust_with_config_dir(
                        &first_run.workspace,
                        &first_run.config_dir,
                        if trusted {
                            TrustLevel::Trusted
                        } else {
                            TrustLevel::Untrusted
                        },
                    ) {
                        state.push_message(ChatMessage::Error(format!(
                            "failed to persist workspace trust: {error}"
                        )));
                        return Ok(SetupFlow::Continue);
                    }
                    first_run.workspace_trusted = trusted;
                }
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
                    // A key is already configured, so there is nothing to save.
                    // Skip the "API key saved" confirmation and enter the main UI.
                    finish_setup(state, action_tx, textarea, vim_state, theme, initial_prompt);
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
                finish_setup(state, action_tx, textarea, vim_state, theme, initial_prompt);
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

/// Leave the first-run flow and enter the main interactive UI. Shared by the
/// "API key saved" confirmation step and the welcome step when a key is already
/// configured, so both paths reset state identically and honor `initial_prompt`.
fn finish_setup(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
) {
    state.set_status(AppStatus::Idle);
    state.setup_step = 0;
    *textarea = make_textarea(vim_state, theme);

    if let Some(prompt) = initial_prompt {
        state.push_message(ChatMessage::User(prompt.clone()));
        state.enter_running();
        let _ = action_tx.send(UserAction::Submit(prompt));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> (Event, KeyEvent) {
        let key = KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        (Event::Key(key), key)
    }

    /// Drive one welcome-step key press against a state with no first-run
    /// context. Trust/Untrusted arms fail closed to `Continue`, so this isolates
    /// the navigation and Exit-dispatch logic under test.
    fn dispatch(state: &mut AppState, code: KeyCode) -> SetupFlow {
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (action_tx, _action_rx) = mpsc::unbounded();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea(&vim_state, &theme);
        let (event, key) = press(code);
        handle_setup_key(
            &event,
            &key,
            state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
            None,
        )
        .expect("handle setup key")
    }

    fn welcome_state() -> AppState {
        let (event_tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            event_tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.setup_step = 0;
        state
    }

    #[test]
    fn arrow_navigation_visits_all_three_options_and_clamps() {
        let mut state = welcome_state();
        assert_eq!(state.setup_selection, SETUP_TRUST_SELECTION);

        dispatch(&mut state, KeyCode::Down);
        assert_eq!(state.setup_selection, SETUP_UNTRUSTED_SELECTION);
        dispatch(&mut state, KeyCode::Down);
        assert_eq!(state.setup_selection, SETUP_EXIT_SELECTION);
        // Clamp at the last option instead of wrapping.
        dispatch(&mut state, KeyCode::Down);
        assert_eq!(state.setup_selection, SETUP_EXIT_SELECTION);

        dispatch(&mut state, KeyCode::Up);
        assert_eq!(state.setup_selection, SETUP_UNTRUSTED_SELECTION);
        dispatch(&mut state, KeyCode::Up);
        assert_eq!(state.setup_selection, SETUP_TRUST_SELECTION);
        dispatch(&mut state, KeyCode::Up);
        assert_eq!(state.setup_selection, SETUP_TRUST_SELECTION);
    }

    #[test]
    fn quick_keys_move_selection_to_matching_option() {
        let mut state = welcome_state();
        dispatch(&mut state, KeyCode::Char('e'));
        assert_eq!(state.setup_selection, SETUP_EXIT_SELECTION);
    }

    #[test]
    fn enter_persists_untrusted_selection() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(workspace.path().to_path_buf());
        folder_trust::set_trust_with_config_dir(workspace.path(), home.path(), TrustLevel::Trusted)
            .unwrap();
        let mut state = welcome_state();
        state.first_run =
            Some(orca_runtime::onboarding::inspect_first_run_in(&config, home.path()).unwrap());
        state.setup_selection = SETUP_UNTRUSTED_SELECTION;
        dispatch(&mut state, KeyCode::Enter);
        assert!(!state.first_run.as_ref().unwrap().workspace_trusted);
        assert_eq!(
            folder_trust::trust_level_with_config_dir(workspace.path(), home.path()),
            Some(TrustLevel::Untrusted),
        );
    }

    #[test]
    fn enter_on_exit_option_requests_exit() {
        let mut state = welcome_state();
        state.setup_selection = SETUP_EXIT_SELECTION;
        assert!(matches!(
            dispatch(&mut state, KeyCode::Enter),
            SetupFlow::Exit(0)
        ));
    }

    #[test]
    fn e_key_requests_exit() {
        let mut state = welcome_state();
        assert!(matches!(
            dispatch(&mut state, KeyCode::Char('E')),
            SetupFlow::Exit(0)
        ));
        assert_eq!(state.setup_selection, SETUP_EXIT_SELECTION);
    }

    #[test]
    fn finish_setup_enters_main_ui_without_confirmation_step() {
        let mut state = welcome_state();
        state.setup_step = 2;
        state.set_status(AppStatus::Setup);
        let (action_tx, _action_rx) = mpsc::unbounded();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea(&vim_state, &theme);

        finish_setup(
            &mut state,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
            None,
        );

        // Lands directly in the idle main UI; the setup flow is fully reset so
        // the "API key saved" confirmation is never shown for an already
        // configured key.
        assert_eq!(state.status, AppStatus::Idle);
        assert_eq!(state.setup_step, 0);
    }

    #[test]
    fn finish_setup_submits_initial_prompt_when_present() {
        let mut state = welcome_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea(&vim_state, &theme);

        finish_setup(
            &mut state,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
            Some("hello".to_string()),
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::Submit(prompt)) if prompt == "hello"
        ));
    }
}
