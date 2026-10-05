//! Keys of the `/mcp` panel. `r`, `l` and `o` hand a reconnect, a login or
//! a logout of the selected server to a worker (see `mcp_server_actions`),
//! and `l` again cancels a login that waits for the browser; the panel's
//! list only ever changes with the catalog: the thread's, or, before the
//! first message, that of the servers that started with the TUI.

use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orca_mcp::McpAuthKind;
use orca_mcp::oauth::McpLoginCancel;

use crate::protocol::UserAction;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, McpActionInFlight};

#[derive(Clone, Copy)]
enum McpServerAction {
    Reconnect,
    LogIn,
    LogOut,
}

pub(crate) fn handle_mcp_dialog_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    let server_count = state.mcp_panel_servers().len();
    let Some(dialog) = state.mcp_dialog.as_mut() else {
        return;
    };
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc => state.mcp_dialog = None,
        KeyCode::Up | KeyCode::BackTab if server_count > 0 => {
            dialog.selected = dialog
                .selected
                .min(server_count - 1)
                .checked_sub(1)
                .unwrap_or(server_count - 1);
        }
        KeyCode::Down | KeyCode::Tab if server_count > 0 => {
            dialog.selected = (dialog.selected.min(server_count - 1) + 1) % server_count;
        }
        KeyCode::Enter => dialog.showing_details = !dialog.showing_details,
        KeyCode::Char('r') if plain => start_action(state, action_tx, McpServerAction::Reconnect),
        KeyCode::Char('l') if plain => start_action(state, action_tx, McpServerAction::LogIn),
        KeyCode::Char('o') if plain => start_action(state, action_tx, McpServerAction::LogOut),
        _ => {}
    }
}

/// Sends `action` for the selected server, unless one is still running for
/// it, which answers first: an older reconnect could otherwise overwrite a
/// newer one. The one exception is `l` on a login that waits for the
/// browser, which cancels it; its worker still ends it, and frees the
/// server. Once the browser is back with the code the login finishes, and
/// `l` says so, and that it can no longer be cancelled. Logging in and out
/// uses the server's config entry, under whose name its login is saved, and
/// only for a server that logs in with OAuth.
fn start_action(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    action: McpServerAction,
) {
    let Some(server) = state.selected_mcp_server() else {
        return;
    };
    if matches!(action, McpServerAction::LogIn)
        && let Some(McpActionInFlight::LoggingIn { cancel, .. }) =
            state.mcp_actions_in_flight.get(&server.key)
    {
        // A login cancelled already is running until its worker ends it,
        // as any other action is: that is answered below.
        let notice = if cancel.cancel() {
            Some(format!("login to MCP server {} cancelled", server.name))
        } else if cancel.is_finishing() {
            Some(format!(
                "login to MCP server {} is finishing and can no longer be cancelled",
                server.name
            ))
        } else {
            None
        };
        if let Some(text) = notice {
            state.push_message(ChatMessage::System {
                text,
                expanded: false,
            });
            return;
        }
    }
    let request = if state.mcp_actions_in_flight.contains_key(&server.key) {
        Err(format!(
            "an MCP action for {} is already running",
            server.name
        ))
    } else {
        match action {
            McpServerAction::Reconnect => Ok((
                UserAction::McpReconnect {
                    server: server.name.clone(),
                },
                McpActionInFlight::Reconnecting,
            )),
            McpServerAction::LogIn | McpServerAction::LogOut => {
                match state.mcp_server_config(&server.key) {
                    None => Err(format!(
                        "no single configured MCP server matches {}",
                        server.name
                    )),
                    Some(config) if McpAuthKind::of(config) != McpAuthKind::OAuth => Err(format!(
                        "MCP server '{}' does not use OAuth login",
                        server.name
                    )),
                    Some(config) if matches!(action, McpServerAction::LogIn) => {
                        let cancel = McpLoginCancel::default();
                        Ok((
                            UserAction::McpLogin {
                                server: config.clone(),
                                cancel: cancel.clone(),
                            },
                            McpActionInFlight::LoggingIn {
                                authorization_url: None,
                                cancel,
                            },
                        ))
                    }
                    Some(config) => Ok((
                        UserAction::McpLogout {
                            server: config.name.clone(),
                        },
                        McpActionInFlight::LoggingOut,
                    )),
                }
            }
        }
    };
    match request {
        Ok((request, running)) => {
            state.mcp_actions_in_flight.insert(server.key, running);
            let _ = action_tx.send(request);
        }
        Err(notice) => state.push_message(ChatMessage::System {
            text: notice,
            expanded: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

    use crate::protocol::TuiEvent;
    use crate::slash_command_actions::handle_slash_command;
    use crate::surface_projection::{McpCatalogView, McpServerStatusView, McpServerView};
    use crate::test_support::{last_notice, press_in_mcp_panel, test_run_config};

    fn server(name: &str, status: McpServerStatusView) -> McpServerView {
        McpServerView {
            name: name.to_string(),
            status,
            prompts_error: None,
        }
    }

    fn remote(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Http,
            url: Some(format!("https://{}.example/mcp", name.to_lowercase())),
            ..McpServerConfig::default()
        }
    }

    fn stdio(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("true".to_string()),
            ..McpServerConfig::default()
        }
    }

    /// An idle TUI whose thread has `servers`, with `/mcp` opened on the
    /// config entries `configs`, and what the TUI sends.
    fn open_panel(
        servers: Vec<McpServerView>,
        configs: Vec<McpServerConfig>,
    ) -> (AppState, mpsc::Receiver<UserAction>) {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "deepseek-v4-pro".to_string(),
            "/tmp/project".to_string(),
        );
        state.mcp_catalog = McpCatalogView {
            servers,
            ..McpCatalogView::default()
        };
        let mut config = test_run_config();
        config.mcp_servers = configs;
        let shared = Arc::new(Mutex::new(config.clone()));
        handle_slash_command("/mcp", &mut config, &shared, &mut state, &action_tx);
        assert!(state.mcp_dialog.is_some(), "/mcp did not open the panel");
        (state, action_rx)
    }

    #[test]
    fn r_sends_a_reconnect_for_the_selected_server() {
        let (mut state, action_rx) = open_panel(
            vec![
                server("docs", McpServerStatusView::Connected),
                server("github", McpServerStatusView::Failed("refused".to_string())),
            ],
            Vec::new(),
        );

        press_in_mcp_panel(&mut state, KeyCode::Down);
        press_in_mcp_panel(&mut state, KeyCode::Char('r'));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpReconnect { server }) if server == "github"
        ));
        assert!(action_rx.try_recv().is_err());
        assert_eq!(
            state.mcp_actions_in_flight.get("github"),
            Some(&McpActionInFlight::Reconnecting)
        );
    }

    #[test]
    fn l_and_o_send_login_and_logout() {
        let (mut state, action_rx) = open_panel(
            vec![server("linear", McpServerStatusView::NeedsLogin)],
            vec![remote("linear")],
        );

        press_in_mcp_panel(&mut state, KeyCode::Char('l'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogin { server, .. }) if server.name == "linear"
        ));

        state.update(TuiEvent::McpActionFinished {
            server: "linear".to_string(),
        });
        press_in_mcp_panel(&mut state, KeyCode::Char('o'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogout { server }) if server == "linear"
        ));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn a_server_with_an_action_running_takes_no_other_until_it_finishes() {
        let (mut state, action_rx) = open_panel(
            vec![server("linear", McpServerStatusView::NeedsLogin)],
            vec![remote("linear")],
        );
        press_in_mcp_panel(&mut state, KeyCode::Char('l'));
        assert!(action_rx.try_recv().is_ok());

        for key in ['r', 'o'] {
            press_in_mcp_panel(&mut state, KeyCode::Char(key));
            assert!(action_rx.try_recv().is_err(), "'{key}' sent an action");
            assert_eq!(
                last_notice(&state),
                Some("an MCP action for linear is already running")
            );
        }
        // `l` cancels the login rather than starting another; once it has,
        // the login runs until it stops, and `l` waits too.
        press_in_mcp_panel(&mut state, KeyCode::Char('l'));
        assert!(action_rx.try_recv().is_err(), "`l` sent an action");
        assert_eq!(
            last_notice(&state),
            Some("login to MCP server linear cancelled")
        );
        press_in_mcp_panel(&mut state, KeyCode::Char('l'));
        assert!(action_rx.try_recv().is_err(), "`l` sent an action");
        assert_eq!(
            last_notice(&state),
            Some("an MCP action for linear is already running")
        );

        // Closing the panel leaves the login running; its end frees the
        // server, failed or not.
        press_in_mcp_panel(&mut state, KeyCode::Esc);
        state.update(TuiEvent::McpActionFinished {
            server: "linear".to_string(),
        });
        assert!(state.mcp_actions_in_flight.is_empty());

        // A running action answers first, before the checks `l` and `o`
        // make of a server that does not log in with OAuth.
        let (mut state, action_rx) = open_panel(
            vec![server("local", McpServerStatusView::Connected)],
            vec![stdio("local")],
        );
        press_in_mcp_panel(&mut state, KeyCode::Char('r'));
        assert!(action_rx.try_recv().is_ok());
        for key in ['l', 'o'] {
            press_in_mcp_panel(&mut state, KeyCode::Char(key));
            assert!(action_rx.try_recv().is_err(), "'{key}' sent an action");
            assert_eq!(
                last_notice(&state),
                Some("an MCP action for local is already running")
            );
        }
    }

    /// Once the browser is back with its code the login finishes, and `l`
    /// says so, rather than that an action is running.
    #[test]
    fn pressing_l_once_the_browser_is_back_says_the_login_is_finishing() {
        let (mut state, action_rx) = open_panel(
            vec![server("linear", McpServerStatusView::NeedsLogin)],
            vec![remote("linear")],
        );
        state.mcp_actions_in_flight.insert(
            "linear".to_string(),
            McpActionInFlight::LoggingIn {
                authorization_url: None,
                cancel: McpLoginCancel::finishing_for_test(),
            },
        );

        press_in_mcp_panel(&mut state, KeyCode::Char('l'));

        assert!(action_rx.try_recv().is_err(), "`l` sent an action");
        assert_eq!(
            last_notice(&state),
            Some("login to MCP server linear is finishing and can no longer be cancelled")
        );
        assert!(
            !state.transcript.messages.iter().any(|message| matches!(
                message,
                ChatMessage::System { text, .. } if text.contains("already running")
            )),
            "{:?}",
            state.transcript.messages
        );
        // The login goes on: `l` did not cancel it.
        assert!(matches!(
            state.mcp_actions_in_flight.get("linear"),
            Some(McpActionInFlight::LoggingIn { cancel, .. }) if cancel.is_finishing()
        ));
    }

    #[test]
    fn servers_that_do_not_log_in_with_oauth_refuse_l_and_o() {
        let mut header = remote("headered");
        header
            .headers
            .insert("authorization".to_string(), "Bearer t".to_string());
        let mut bearer = remote("beared");
        bearer.bearer_token_env_var = Some("TOKEN".to_string());
        let (mut state, action_rx) = open_panel(
            vec![
                server("local", McpServerStatusView::Connected),
                server("headered", McpServerStatusView::Connected),
                server("beared", McpServerStatusView::Connected),
            ],
            vec![stdio("local"), header, bearer],
        );

        for name in ["local", "headered", "beared"] {
            for key in ['l', 'o'] {
                press_in_mcp_panel(&mut state, KeyCode::Char(key));
                assert!(action_rx.try_recv().is_err(), "'{key}' on {name}");
                assert_eq!(
                    last_notice(&state),
                    Some(format!("MCP server '{name}' does not use OAuth login").as_str())
                );
            }
            press_in_mcp_panel(&mut state, KeyCode::Down);
        }
        assert!(state.mcp_actions_in_flight.is_empty());
    }

    #[test]
    fn a_server_logs_in_and_out_under_its_config_name() {
        let (mut state, action_rx) = open_panel(
            vec![server("my_server", McpServerStatusView::NeedsLogin)],
            vec![remote("My-Server")],
        );
        let shown = crate::test_support::frame_string(&mut state, 100, 30);
        assert!(shown.contains("My-Server"), "{shown}");
        assert!(!shown.contains("my_server"), "{shown}");

        press_in_mcp_panel(&mut state, KeyCode::Char('l'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogin { server, .. }) if server.name == "My-Server"
        ));
        assert!(state.mcp_actions_in_flight.contains_key("my_server"));

        state.update(TuiEvent::McpActionFinished {
            server: "my_server".to_string(),
        });
        press_in_mcp_panel(&mut state, KeyCode::Char('o'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogout { server }) if server == "My-Server"
        ));
    }

    #[test]
    fn before_the_first_message_the_panel_acts_on_the_servers_that_started() {
        // No thread yet: the catalog is that of the servers that started
        // with the TUI.
        let (mut state, action_rx) = open_panel(Vec::new(), vec![stdio("local")]);
        assert!(state.mcp_panel_servers().is_empty());
        state.update(TuiEvent::McpCatalogPrestart(McpCatalogView {
            servers: vec![server(
                "local",
                McpServerStatusView::Failed("MCP server closed stdout".to_string()),
            )],
            ..McpCatalogView::default()
        }));
        assert!(!state.surface_mcp_catalog_applied);

        press_in_mcp_panel(&mut state, KeyCode::Char('r'));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpReconnect { server }) if server == "local"
        ));
        assert_eq!(
            state.mcp_actions_in_flight.get("local"),
            Some(&McpActionInFlight::Reconnecting)
        );
    }

    #[test]
    fn enter_toggles_details_and_esc_closes_the_panel_from_either_view() {
        for open_details in [false, true] {
            let (mut state, action_rx) = open_panel(
                vec![server("docs", McpServerStatusView::Connected)],
                Vec::new(),
            );
            if open_details {
                press_in_mcp_panel(&mut state, KeyCode::Enter);
                assert!(
                    state
                        .mcp_dialog
                        .is_some_and(|dialog| dialog.showing_details)
                );
            }

            press_in_mcp_panel(&mut state, KeyCode::Esc);

            assert!(state.mcp_dialog.is_none());
            assert!(action_rx.try_recv().is_err());
        }

        let (mut state, _action_rx) = open_panel(
            vec![server("docs", McpServerStatusView::Connected)],
            Vec::new(),
        );
        press_in_mcp_panel(&mut state, KeyCode::Enter);
        press_in_mcp_panel(&mut state, KeyCode::Enter);
        assert!(
            state
                .mcp_dialog
                .is_some_and(|dialog| !dialog.showing_details)
        );
    }
}
