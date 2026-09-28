//! Keys of the `/mcp` panel. `r`, `l` and `o` hand a reconnect, a login or
//! a logout of the selected server to a worker (see `mcp_server_actions`);
//! the panel's list only ever changes with the catalog the next projection
//! brings. Before the conversation's runtime starts, the panel lists the
//! config's servers, which can be logged in to and out of but not yet
//! reconnected.

use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orca_mcp::McpAuthKind;

use crate::mcp_server_actions::MCP_SERVERS_NOT_STARTED;
use crate::protocol::UserAction;
use crate::transcript_state::ChatMessage;
use crate::types::AppState;

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
/// it: an older reconnect could otherwise overwrite a newer one. Logging in
/// and out uses the server's config entry, under whose name its login is
/// saved, and only for a server that logs in with OAuth.
fn start_action(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    action: McpServerAction,
) {
    let Some(server) = state
        .selected_mcp_server()
        .map(|server| server.name.clone())
    else {
        return;
    };
    let name = state.mcp_server_display_name(&server).to_string();
    let request = match action {
        McpServerAction::Reconnect if state.mcp_servers_before_start() => {
            Err(MCP_SERVERS_NOT_STARTED.to_string())
        }
        McpServerAction::Reconnect => Ok(UserAction::McpReconnect {
            server: name.clone(),
        }),
        McpServerAction::LogIn | McpServerAction::LogOut => {
            match state.mcp_server_config(&server) {
                None => Err(format!("no single configured MCP server matches {name}")),
                Some(config) if McpAuthKind::of(config) != McpAuthKind::OAuth => {
                    Err(format!("MCP server '{name}' does not use OAuth login"))
                }
                Some(config) if matches!(action, McpServerAction::LogIn) => {
                    Ok(UserAction::McpLogin {
                        server: config.clone(),
                    })
                }
                Some(config) => Ok(UserAction::McpLogout {
                    server: config.name.clone(),
                }),
            }
        }
    };
    let request = request.and_then(|request| {
        if state.mcp_actions_in_flight.insert(server) {
            Ok(request)
        } else {
            Err(format!("an MCP action for {name} is already running"))
        }
    });
    match request {
        Ok(request) => {
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
    use crate::test_support::test_run_config;

    fn server(name: &str, status: McpServerStatusView) -> McpServerView {
        McpServerView {
            name: name.to_string(),
            status,
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
    /// config entries `configs`.
    fn open_panel(
        servers: Vec<McpServerView>,
        configs: Vec<McpServerConfig>,
    ) -> (
        AppState,
        mpsc::Sender<UserAction>,
        mpsc::Receiver<UserAction>,
    ) {
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
        (state, action_tx, action_rx)
    }

    fn press(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>, code: KeyCode) {
        handle_mcp_dialog_key(&KeyEvent::new(code, KeyModifiers::NONE), state, action_tx);
    }

    /// The whole screen, as rows of text.
    fn screen(state: &mut AppState) -> String {
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = tui_textarea::TextArea::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, state, &textarea, &theme))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn last_notice(state: &AppState) -> Option<&str> {
        match state.transcript.messages.last() {
            Some(ChatMessage::System { text, .. }) => Some(text.as_str()),
            _ => None,
        }
    }

    #[test]
    fn r_sends_a_reconnect_for_the_selected_server() {
        let (mut state, action_tx, action_rx) = open_panel(
            vec![
                server("docs", McpServerStatusView::Connected),
                server("github", McpServerStatusView::Failed("refused".to_string())),
            ],
            Vec::new(),
        );

        press(&mut state, &action_tx, KeyCode::Down);
        press(&mut state, &action_tx, KeyCode::Char('r'));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpReconnect { server }) if server == "github"
        ));
        assert!(action_rx.try_recv().is_err());
        assert!(state.mcp_actions_in_flight.contains("github"));
    }

    #[test]
    fn l_and_o_send_login_and_logout() {
        let (mut state, action_tx, action_rx) = open_panel(
            vec![server("linear", McpServerStatusView::NeedsLogin)],
            vec![remote("linear")],
        );

        press(&mut state, &action_tx, KeyCode::Char('l'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogin { server }) if server.name == "linear"
        ));

        state.update(TuiEvent::McpActionFinished {
            server: "linear".to_string(),
        });
        press(&mut state, &action_tx, KeyCode::Char('o'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogout { server }) if server == "linear"
        ));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn a_server_with_an_action_running_takes_no_other_until_it_finishes() {
        let (mut state, action_tx, action_rx) = open_panel(
            vec![server("linear", McpServerStatusView::NeedsLogin)],
            vec![remote("linear")],
        );
        press(&mut state, &action_tx, KeyCode::Char('l'));
        assert!(action_rx.try_recv().is_ok());

        for key in ['r', 'l', 'o'] {
            press(&mut state, &action_tx, KeyCode::Char(key));
            assert!(action_rx.try_recv().is_err(), "'{key}' sent an action");
            assert_eq!(
                last_notice(&state),
                Some("an MCP action for linear is already running")
            );
        }

        // Closing the panel leaves the login running; its end frees the
        // server, failed or not.
        press(&mut state, &action_tx, KeyCode::Esc);
        state.update(TuiEvent::McpActionFinished {
            server: "linear".to_string(),
        });
        assert!(state.mcp_actions_in_flight.is_empty());
    }

    #[test]
    fn servers_that_do_not_log_in_with_oauth_refuse_l_and_o() {
        let mut header = remote("headered");
        header
            .headers
            .insert("authorization".to_string(), "Bearer t".to_string());
        let mut bearer = remote("beared");
        bearer.bearer_token_env_var = Some("TOKEN".to_string());
        let (mut state, action_tx, action_rx) = open_panel(
            vec![
                server("local", McpServerStatusView::Connected),
                server("headered", McpServerStatusView::Connected),
                server("beared", McpServerStatusView::Connected),
            ],
            vec![stdio("local"), header, bearer],
        );

        for name in ["local", "headered", "beared"] {
            for key in ['l', 'o'] {
                press(&mut state, &action_tx, KeyCode::Char(key));
                assert!(action_rx.try_recv().is_err(), "'{key}' on {name}");
                assert_eq!(
                    last_notice(&state),
                    Some(format!("MCP server '{name}' does not use OAuth login").as_str())
                );
            }
            press(&mut state, &action_tx, KeyCode::Down);
        }
        assert!(state.mcp_actions_in_flight.is_empty());
    }

    #[test]
    fn a_server_logs_in_and_out_under_its_config_name() {
        let (mut state, action_tx, action_rx) = open_panel(
            vec![server("my_server", McpServerStatusView::NeedsLogin)],
            vec![remote("My-Server")],
        );
        let shown = screen(&mut state);
        assert!(shown.contains("My-Server"), "{shown}");
        assert!(!shown.contains("my_server"), "{shown}");

        press(&mut state, &action_tx, KeyCode::Char('l'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogin { server }) if server.name == "My-Server"
        ));
        assert!(state.mcp_actions_in_flight.contains("my_server"));

        state.update(TuiEvent::McpActionFinished {
            server: "my_server".to_string(),
        });
        press(&mut state, &action_tx, KeyCode::Char('o'));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::McpLogout { server }) if server == "My-Server"
        ));
    }

    #[test]
    fn r_before_the_runtime_starts_says_when_servers_connect_and_sends_nothing() {
        // No catalog yet: the panel lists the config's servers.
        let (mut state, action_tx, action_rx) = open_panel(Vec::new(), vec![remote("linear")]);

        press(&mut state, &action_tx, KeyCode::Char('r'));

        assert!(action_rx.try_recv().is_err());
        assert_eq!(
            last_notice(&state),
            Some("MCP servers connect when the conversation starts; send a message first.")
        );
        assert!(state.mcp_actions_in_flight.is_empty());
    }

    #[test]
    fn enter_toggles_details_and_esc_closes_the_panel_from_either_view() {
        for open_details in [false, true] {
            let (mut state, action_tx, action_rx) = open_panel(
                vec![server("docs", McpServerStatusView::Connected)],
                Vec::new(),
            );
            if open_details {
                press(&mut state, &action_tx, KeyCode::Enter);
                assert!(
                    state
                        .mcp_dialog
                        .is_some_and(|dialog| dialog.showing_details)
                );
            }

            press(&mut state, &action_tx, KeyCode::Esc);

            assert!(state.mcp_dialog.is_none());
            assert!(action_rx.try_recv().is_err());
        }

        let (mut state, action_tx, _action_rx) = open_panel(
            vec![server("docs", McpServerStatusView::Connected)],
            Vec::new(),
        );
        press(&mut state, &action_tx, KeyCode::Enter);
        press(&mut state, &action_tx, KeyCode::Enter);
        assert!(
            state
                .mcp_dialog
                .is_some_and(|dialog| !dialog.showing_details)
        );
    }
}
