//! The `/mcp` panel: one row per MCP server with its status, or the selected
//! server's tools, prompts and tool filters. The renderer draws it over the
//! conversation while `AppState::mcp_dialog` is set; its keys are handled in
//! `mcp_dialog_actions`.

use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::types::AppState;

/// The `/mcp` panel's keys, the same over the list and the details.
const MCP_DIALOG_HINTS: [(&str, &str); 6] = [
    ("↑↓", "select"),
    ("Enter", "details"),
    ("r", "reconnect"),
    ("l", "log in"),
    ("o", "log out"),
    ("Esc", "close"),
];

/// `/mcp`: one row per server with its status, or the selected server's
/// tools, prompts and tool filters. A window too short for all of it keeps
/// the selected server's row and the keys.
pub(crate) fn render_mcp_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(dialog) = state.mcp_dialog else {
        return;
    };
    let area = frame.area();
    let width = 84u16.min(area.width.saturating_sub(4));
    let inner_width = usize::from(width.saturating_sub(4));
    // In an 80-column window the keys take two rows rather than lose some.
    let keys =
        if crate::chrome::hint_line(theme, usize::MAX, &MCP_DIALOG_HINTS).width() <= inner_width {
            vec![crate::chrome::hint_line(
                theme,
                inner_width,
                &MCP_DIALOG_HINTS,
            )]
        } else {
            MCP_DIALOG_HINTS
                .chunks(3)
                .map(|keys| crate::chrome::hint_line(theme, inner_width, keys))
                .collect()
        };
    // Rows inside the borders of a dialog with a free row above and below,
    // less the blank row and the keys under the body.
    let body_rows = usize::from(area.height.saturating_sub(4))
        .saturating_sub(1 + keys.len())
        .max(1);
    let mut lines = match state.selected_mcp_server() {
        Some(server) if dialog.showing_details => {
            mcp_server_details(state, &server, theme, inner_width, body_rows)
        }
        _ => mcp_server_rows(state, dialog.selected, theme, inner_width, body_rows),
    };
    lines.push(Line::from(""));
    lines.extend(keys);
    let popup = crate::chrome::dialog_rect(area, width, lines.len() as u16, area.height);
    frame.render_widget(Clear, popup);
    let block = crate::chrome::panel_block(theme, "MCP servers", theme.border);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The server list, scrolled so the selected server shows.
fn mcp_server_rows(
    state: &AppState,
    selected: usize,
    theme: &Theme,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let servers = state.mcp_panel_servers();
    if servers.is_empty() {
        return vec![Line::from(Span::styled(
            "No MCP servers. Add one with 'orca mcp add'.",
            theme.muted_style(),
        ))];
    }
    let selected = selected.min(servers.len() - 1);
    let label_width = servers
        .iter()
        .map(|server| UnicodeWidthStr::width(server.name.as_str()))
        .max()
        .unwrap_or(0)
        .min(width / 3);
    servers
        .iter()
        .enumerate()
        .skip((selected + 1).saturating_sub(rows))
        .take(rows)
        .map(|(index, server)| {
            // One row each: a failure's reason is cut to what fits, and the
            // details show it whole.
            crate::chrome::option_line(
                theme,
                index == selected,
                "",
                &server.name,
                label_width,
                &mcp_server_status_text(state, server),
                width,
            )
        })
        .collect()
}

/// While an action runs for the server, what it does: `reconnecting…`,
/// `waiting for browser login… (l to cancel)`, `cancelling login…` or
/// `logging out…`. Otherwise
/// `connected · 2 tools`, `failed: {message}`, `needs login`, `disabled` or
/// `starting`, on one line whatever the message holds.
fn mcp_server_status_text(state: &AppState, server: &crate::types::McpPanelServer) -> String {
    if let Some(action) = state.mcp_actions_in_flight.get(&server.key) {
        return action.label().to_string();
    }
    let tools = state.mcp_catalog.server_tools(&server.key).count();
    server
        .status
        .summary(Some(tools))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The server's status in full, with the authorization url a login waits
/// on; its tools, each with the name a permission rule gives it, marked
/// `read-only` in front, so that a long name cannot push the mark out of
/// the panel; its prompts, each with its arguments; why its prompts could
/// not be listed; and the tool filters its config entry sets. Until a
/// server first connects, its tools and prompts are not known yet.
fn mcp_server_details(
    state: &AppState,
    server: &crate::types::McpPanelServer,
    theme: &Theme,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let catalog = &state.mcp_catalog;
    let tools = catalog
        .server_tools(&server.key)
        .map(|tool| {
            (
                tool.name.as_str(),
                if tool.read_only {
                    format!("read-only · {}", tool.rule_name)
                } else {
                    tool.rule_name.clone()
                },
            )
        })
        .collect::<Vec<_>>();
    // As the prompt's usage writes it, `review_pr <pr> [branch]`, then its
    // description, on one line.
    let prompts = catalog
        .server_prompts(&server.key)
        .map(|prompt| {
            let arguments = crate::commands::mcp_prompt_argument_hint(prompt);
            let usage = if arguments.is_empty() {
                prompt.name.clone()
            } else {
                format!("{} {arguments}", prompt.name)
            };
            let description = prompt
                .description
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (usage, description)
        })
        .collect::<Vec<_>>();
    let prompts_error = catalog
        .servers
        .iter()
        .find(|listed| listed.name == server.key)
        .and_then(|listed| listed.prompts_error.as_deref());
    let config = state.mcp_server_config(&server.key);
    let filter = |tools: &Option<Vec<String>>| {
        tools.as_ref().map(|tools| {
            if tools.is_empty() {
                "none".to_string()
            } else {
                tools.join(", ")
            }
        })
    };
    let filters = [
        (
            "enabled_tools",
            config.and_then(|config| filter(&config.enabled_tools)),
        ),
        (
            "disabled_tools",
            config.and_then(|config| filter(&config.disabled_tools)),
        ),
    ]
    .into_iter()
    .filter_map(|(label, tools)| Some((label, tools?)))
    .collect::<Vec<_>>();
    let heading = |title: &str| {
        Line::from(Span::styled(
            title.to_string(),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ))
    };
    let muted = |text: &str| {
        crate::display_text::wrap_to_display_width(text, width)
            .into_iter()
            .map(|text| Line::from(Span::styled(text, theme.muted_style())))
            .collect::<Vec<_>>()
    };
    // Each part lines its rows up on its own: a prompt's arguments do not
    // push the tools' rule names to the panel's edge. Among the tools, the
    // rule names get the room they need first, and the tools' own names give
    // way, down to none at all.
    let row = |label: &str, column: usize, detail: &str| {
        crate::chrome::option_line(theme, false, "", label, column, detail, width)
    };
    let widest_rule = tools
        .iter()
        .map(|(_, rule_name)| UnicodeWidthStr::width(rule_name.as_str()))
        .max()
        .unwrap_or(0);
    let tools_column = mcp_details_column(tools.iter().map(|(name, _)| *name), width)
        .min(width.saturating_sub(MCP_DETAILS_ROW_CHROME + widest_rule));
    let prompts_column = mcp_details_column(prompts.iter().map(|(usage, _)| usage.as_str()), width);
    let filters_column = mcp_details_column(filters.iter().map(|(label, _)| *label), width);

    let mut lines = vec![Line::from(Span::styled(
        server.name.clone(),
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(muted(&mcp_server_status_text(state, server)));
    // In full, broken wherever it must: the user may have to open it by hand.
    if let Some(crate::types::McpActionInFlight::LoggingIn {
        authorization_url: Some(url),
        ..
    }) = state.mcp_actions_in_flight.get(&server.key)
    {
        lines.push(Line::from(Span::styled(
            "If your browser does not open, visit:",
            theme.muted_style(),
        )));
        lines.extend(
            crate::display_text::wrap_to_display_width(url, width)
                .into_iter()
                .map(|text| Line::from(Span::styled(text, Style::default().fg(theme.text)))),
        );
    }
    lines.push(Line::from(""));
    // A stdio server being reconnected is starting too, and keeps its tools
    // meanwhile.
    if server.status == crate::surface_projection::McpServerStatusView::Starting
        && tools.is_empty()
        && prompts.is_empty()
    {
        lines.extend(muted("Tools and prompts appear once the server connects."));
    } else if tools.is_empty() {
        lines.extend(muted("No tools."));
    } else {
        let mut tools_heading = heading("Tools");
        tools_heading.spans.push(Span::styled(
            " · permission rules use the names on the right",
            theme.muted_style(),
        ));
        lines.push(tools_heading);
        // A tool's own name gives way before its rule name does.
        lines.extend(tools.iter().map(|(name, rule_name)| {
            row(
                &crate::display_text::truncate_to_display_width(name, tools_column),
                tools_column,
                rule_name,
            )
        }));
    }
    if let Some(reason) = prompts_error {
        lines.extend(muted(&format!("prompts unavailable: {reason}")));
    }
    if !prompts.is_empty() {
        lines.push(heading("Prompts"));
        // Its arguments before its description, up to the panel's edge.
        lines.extend(prompts.iter().map(|(usage, description)| {
            row(
                &crate::display_text::truncate_to_display_width(
                    usage,
                    width.saturating_sub(MCP_DETAILS_ROW_CHROME),
                ),
                prompts_column,
                description,
            )
        }));
    }
    if !filters.is_empty() {
        lines.push(heading("Tool filters"));
        lines.extend(
            filters
                .iter()
                .map(|(label, tools)| row(label, filters_column, tools)),
        );
    }
    if lines.len() > rows {
        lines.truncate(rows.saturating_sub(1));
        lines.push(Line::from(Span::styled("…", theme.muted_style())));
    }
    lines
}

/// What a `/mcp` details row takes besides its two columns: the marker, a
/// space, and the gap between the columns (see `chrome::option_line`).
const MCP_DETAILS_ROW_CHROME: usize = 4;

/// How wide the first column of the `/mcp` details rows labelled `labels`
/// is: as wide as the widest label, up to half of `width`.
fn mcp_details_column<'a>(labels: impl Iterator<Item = &'a str>, width: usize) -> usize {
    labels
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(width / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TuiEvent;
    use crate::test_support::frame_string;
    use crossbeam_channel as mpsc;
    use std::sync::{Arc, Mutex};

    fn test_state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0".to_string(),
            "deepseek".to_string(),
            "/tmp".to_string(),
        )
    }

    fn mcp_server_view(
        name: &str,
        status: crate::surface_projection::McpServerStatusView,
    ) -> crate::surface_projection::McpServerView {
        crate::surface_projection::McpServerView {
            name: name.to_string(),
            status,
            prompts_error: None,
        }
    }

    fn mcp_tool_view(
        server: &str,
        name: &str,
        read_only: bool,
    ) -> crate::surface_projection::McpToolView {
        crate::surface_projection::McpToolView {
            server: server.to_string(),
            name: name.to_string(),
            rule_name: format!(
                "mcp__{server}__{}",
                orca_core::mcp_types::canonical_mcp_name(name)
            ),
            read_only,
        }
    }

    #[test]
    fn slash_mcp_opens_the_panel_with_each_server_status() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![
                mcp_server_view("docs", McpServerStatusView::Connected),
                mcp_server_view(
                    "github",
                    McpServerStatusView::Failed("connection refused".to_string()),
                ),
                mcp_server_view("linear", McpServerStatusView::NeedsLogin),
                mcp_server_view("archive", McpServerStatusView::Disabled),
            ],
            tools: vec![
                mcp_tool_view("docs", "search", true),
                mcp_tool_view("docs", "fetch", false),
            ],
            prompts: Vec::new(),
        };
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _action_rx) = mpsc::unbounded();

        crate::slash_command_actions::handle_slash_command(
            "/mcp",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        let frame = frame_string(&mut state, 100, 30);

        for (server, status) in [
            ("docs", "connected · 2 tools"),
            ("github", "failed: connection refused"),
            ("linear", "needs login"),
            ("archive", "disabled"),
        ] {
            assert!(
                frame
                    .lines()
                    .any(|line| line.contains(server) && line.contains(status)),
                "no row for {server} reads {status}:\n{frame}"
            );
        }
        assert!(frame.contains("› docs"), "{frame}");
        assert!(
            frame.contains(
                "↑↓ select · Enter details · r reconnect · l log in · o log out · Esc close"
            ),
            "{frame}"
        );
    }

    #[test]
    fn the_mcp_keys_take_two_rows_in_an_80_column_window() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![mcp_server_view("docs", McpServerStatusView::Connected)],
            ..Default::default()
        };
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: false,
        });

        let frame = frame_string(&mut state, 80, 24);

        assert!(
            frame.contains("↑↓ select · Enter details · r reconnect"),
            "{frame}"
        );
        assert!(
            frame.contains("l log in · o log out · Esc close"),
            "{frame}"
        );
    }

    #[test]
    fn a_server_still_connecting_says_its_tools_come_once_it_does() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        let remote = |name: &str| orca_core::mcp_types::McpServerConfig {
            name: name.to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Http,
            url: Some("https://mcp.example/mcp".to_string()),
            ..Default::default()
        };
        let mut config = crate::test_support::test_run_config();
        config.mcp_servers = vec![
            orca_core::mcp_types::McpServerConfig {
                enabled_tools: Some(vec!["list_issues".to_string()]),
                ..remote("My-Server")
            },
            orca_core::mcp_types::McpServerConfig {
                disabled: true,
                ..remote("archive")
            },
        ];
        // No message sent yet: the catalog is that of the servers that
        // started with the TUI, one still connecting.
        state.update(TuiEvent::McpCatalogPrestart(
            crate::surface_projection::McpCatalogView {
                servers: vec![
                    mcp_server_view("my_server", McpServerStatusView::Starting),
                    mcp_server_view("archive", McpServerStatusView::Disabled),
                ],
                ..Default::default()
            },
        ));
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _action_rx) = mpsc::unbounded();
        crate::slash_command_actions::handle_slash_command(
            "/mcp",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        let frame = frame_string(&mut state, 100, 30);

        assert!(
            frame
                .lines()
                .any(|line| line.contains("› My-Server") && line.contains("starting")),
            "{frame}"
        );
        assert!(
            frame
                .lines()
                .any(|line| line.contains("archive") && line.contains("disabled")),
            "{frame}"
        );
        assert!(!frame.contains("not connected yet"), "{frame}");

        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: true,
        });
        let frame = frame_string(&mut state, 100, 30);
        assert!(frame.contains("My-Server"), "{frame}");
        assert!(
            frame.contains("Tools and prompts appear once the server connects."),
            "{frame}"
        );
        assert!(!frame.contains("No tools."), "{frame}");
        assert!(
            frame
                .lines()
                .any(|line| line.contains("enabled_tools") && line.contains("list_issues")),
            "{frame}"
        );
    }

    fn servers_with_running_actions() -> AppState {
        use crate::surface_projection::McpServerStatusView;
        use crate::types::McpActionInFlight;
        let mut state = test_state();
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![
                mcp_server_view("linear", McpServerStatusView::NeedsLogin),
                mcp_server_view("docs", McpServerStatusView::Connected),
                mcp_server_view("github", McpServerStatusView::Connected),
            ],
            ..Default::default()
        };
        state.mcp_actions_in_flight.extend([
            (
                "linear".to_string(),
                McpActionInFlight::LoggingIn {
                    authorization_url: None,
                    cancel: Default::default(),
                },
            ),
            ("docs".to_string(), McpActionInFlight::Reconnecting),
            ("github".to_string(), McpActionInFlight::LoggingOut),
        ]);
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: false,
        });
        state
    }

    #[test]
    fn a_running_action_shows_in_its_row_and_details() {
        let mut state = servers_with_running_actions();

        let frame = frame_string(&mut state, 100, 30);
        for (server, running) in [
            ("linear", "waiting for browser login…"),
            ("docs", "reconnecting…"),
            ("github", "logging out…"),
        ] {
            assert!(
                frame
                    .lines()
                    .any(|line| line.contains(server) && line.contains(running)),
                "no row for {server} reads {running}:\n{frame}"
            );
        }
        assert!(!frame.contains("needs login"), "{frame}");

        for (selected, running) in [
            "waiting for browser login…",
            "reconnecting…",
            "logging out…",
        ]
        .into_iter()
        .enumerate()
        {
            state.mcp_dialog = Some(crate::types::McpDialog {
                selected,
                showing_details: true,
            });
            let frame = frame_string(&mut state, 100, 30);
            assert!(frame.contains(running), "{frame}");
        }
    }

    #[test]
    fn a_waiting_login_shows_its_url_in_the_details() {
        let mut state = servers_with_running_actions();
        let url = format!(
            "https://auth.example/authorize?response_type=code&client_id=orca&state={}",
            "0123456789abcdef".repeat(8)
        );
        state.update(TuiEvent::McpLoginUrl {
            server: "linear".to_string(),
            url: url.clone(),
        });
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: true,
        });

        let frame = frame_string(&mut state, 100, 30);

        // The url is wider than the panel: it is broken over rows, in full.
        let rows = frame
            .lines()
            .filter_map(|line| line.split('│').nth(1).map(str::trim))
            .collect::<Vec<_>>();
        let visit = rows
            .iter()
            .position(|row| *row == "If your browser does not open, visit:")
            .unwrap_or_else(|| panic!("no url shown:\n{frame}"));
        let shown = rows[visit + 1..]
            .iter()
            .take_while(|row| !row.is_empty())
            .copied()
            .collect::<String>();
        assert_eq!(shown, url, "{frame}");
    }

    #[test]
    fn a_finished_action_gives_its_row_the_catalog_status_back() {
        let mut state = servers_with_running_actions();
        let frame = frame_string(&mut state, 100, 30);
        assert!(frame.contains("waiting for browser login…"), "{frame}");

        state.update(TuiEvent::McpActionFinished {
            server: "linear".to_string(),
        });

        let frame = frame_string(&mut state, 100, 30);
        assert!(!frame.contains("waiting for browser login…"), "{frame}");
        assert!(
            frame
                .lines()
                .any(|line| line.contains("linear") && line.contains("needs login")),
            "{frame}"
        );
    }

    #[test]
    fn entries_sharing_a_name_show_as_the_one_server_they_name() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        let remote = |name: &str| orca_core::mcp_types::McpServerConfig {
            name: name.to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Http,
            url: Some("https://mcp.example/mcp".to_string()),
            ..Default::default()
        };
        let mut config = crate::test_support::test_run_config();
        config.mcp_servers = vec![remote("My-Server"), remote("my_server")];
        // The registry connects the first, under the name both make.
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![mcp_server_view(
                "my_server",
                McpServerStatusView::NeedsLogin,
            )],
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();
        crate::slash_command_actions::handle_slash_command(
            "/mcp",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );

        // Either entry could be the one the name means, so the panel shows
        // the catalog's name, and asks which to log in to.
        let frame = frame_string(&mut state, 100, 30);
        assert!(
            frame
                .lines()
                .any(|line| line.contains("› my_server") && line.contains("needs login")),
            "{frame}"
        );
        assert!(!frame.contains("My-Server"), "{frame}");
        crate::mcp_dialog_actions::handle_mcp_dialog_key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('l'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &mut state,
            &action_tx,
        );
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            state.transcript.messages.last(),
            Some(crate::transcript_state::ChatMessage::System { text, .. })
                if text == "no single configured MCP server matches my_server"
        ));
    }

    #[test]
    fn a_long_failure_stays_on_its_server_row() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        let reason = "failed to connect:\n".to_string() + &"connection refused ".repeat(12);
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![
                mcp_server_view("github", McpServerStatusView::Failed(reason)),
                mcp_server_view("linear", McpServerStatusView::NeedsLogin),
            ],
            ..Default::default()
        };
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: false,
        });

        let frame = frame_string(&mut state, 100, 30);

        let github = frame
            .lines()
            .position(|line| line.contains("github"))
            .expect("github row");
        let lines = frame.lines().collect::<Vec<_>>();
        assert!(
            lines[github].contains("failed: failed to connect: connection refused"),
            "{frame}"
        );
        assert!(lines[github].contains('…'), "{frame}");
        assert!(lines[github + 1].contains("linear"), "{frame}");
    }

    #[test]
    fn details_mark_read_only_tools_and_list_prompts() {
        use crate::surface_projection::{McpPromptView, McpServerStatusView};
        let mut state = test_state();
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![
                mcp_server_view("github", McpServerStatusView::Connected),
                mcp_server_view("docs", McpServerStatusView::Connected),
            ],
            tools: vec![
                mcp_tool_view("github", "list_issues", true),
                mcp_tool_view("github", "create_issue", false),
                mcp_tool_view("github", "deleteRepo", false),
                mcp_tool_view("docs", "search_docs", true),
            ],
            prompts: vec![McpPromptView {
                server: "github".to_string(),
                name: "review_pr".to_string(),
                description: Some("Review a pull request".to_string()),
                arguments: vec![("number".to_string(), true)],
            }],
        };
        state.mcp_server_configs = vec![
            orca_core::mcp_types::McpServerConfig {
                name: "github".to_string(),
                enabled_tools: Some(vec!["list_issues".to_string(), "create_issue".to_string()]),
                disabled_tools: Some(vec!["delete_repo".to_string()]),
                ..Default::default()
            },
            orca_core::mcp_types::McpServerConfig {
                name: "docs".to_string(),
                ..Default::default()
            },
        ];
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: true,
        });

        let frame = frame_string(&mut state, 100, 30);
        let row = |text: &str| {
            frame
                .lines()
                .find(|line| line.contains(text))
                .unwrap_or_else(|| panic!("no row shows {text}:\n{frame}"))
                .to_string()
        };

        assert!(
            row("list_issues").contains("read-only · mcp__github__list_issues"),
            "{frame}"
        );
        assert!(
            row("create_issue").contains("mcp__github__create_issue"),
            "{frame}"
        );
        assert!(!row("create_issue").contains("read-only"), "{frame}");
        // A rule names a tool as Orca does, which is not always the server's
        // own spelling.
        assert!(
            row("deleteRepo").contains("mcp__github__deleterepo"),
            "{frame}"
        );
        assert!(
            row("Tools").contains("permission rules use the names on the right"),
            "{frame}"
        );
        assert!(
            row("review_pr").contains("Review a pull request"),
            "{frame}"
        );
        assert!(
            row("enabled_tools").contains("list_issues, create_issue"),
            "{frame}"
        );
        assert!(row("disabled_tools").contains("delete_repo"), "{frame}");
        assert!(!frame.contains("search_docs"), "{frame}");

        // A server without tool filters shows neither.
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 1,
            showing_details: true,
        });
        let frame = frame_string(&mut state, 100, 30);
        assert!(frame.contains("search_docs"), "{frame}");
        assert!(!frame.contains("enabled_tools"), "{frame}");
        assert!(!frame.contains("disabled_tools"), "{frame}");
    }

    /// `/mcp` open on the details of the `github` server, whose tools are
    /// `list_issues` and `search_repositories_by_topic` (read-only) and
    /// `create_issue`, and whose prompts are `review_pr <pr> [branch]` and
    /// `summarize_discussion <discussion> [since] [until] [format]`.
    fn github_details() -> AppState {
        use crate::surface_projection::{McpPromptView, McpServerStatusView};
        let mut state = test_state();
        let prompt = |name: &str, description: &str, arguments: &[(&str, bool)]| McpPromptView {
            server: "github".to_string(),
            name: name.to_string(),
            description: Some(description.to_string()),
            arguments: arguments
                .iter()
                .map(|(name, required)| (name.to_string(), *required))
                .collect(),
        };
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![mcp_server_view("github", McpServerStatusView::Connected)],
            tools: vec![
                mcp_tool_view("github", "list_issues", true),
                mcp_tool_view("github", "create_issue", false),
                mcp_tool_view("github", "search_repositories_by_topic", true),
            ],
            prompts: vec![
                prompt(
                    "review_pr",
                    "Review a\npull request",
                    &[("pr", true), ("branch", false)],
                ),
                prompt(
                    "summarize_discussion",
                    "Summarize a discussion",
                    &[
                        ("discussion", true),
                        ("since", false),
                        ("until", false),
                        ("format", false),
                    ],
                ),
            ],
        };
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: true,
        });
        state
    }

    /// The rows of `frame` inside the `/mcp` panel, without its borders.
    fn panel_rows(frame: &str) -> Vec<&str> {
        frame
            .lines()
            .filter_map(|line| {
                let inside = line.split_once('│')?.1;
                Some(inside.rsplit_once('│')?.0.trim())
            })
            .collect()
    }

    #[test]
    fn details_put_read_only_before_the_rule_name() {
        let mut state = github_details();

        let frame = frame_string(&mut state, 80, 30);

        let rows = panel_rows(&frame);
        let row = |text: &str| {
            rows.iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no row shows {text}:\n{frame}"))
        };
        // In full, though a prompt's arguments and a tool's name are long:
        // that name gives way instead.
        assert!(
            row("list_issues").ends_with("read-only · mcp__github__list_issues"),
            "{frame}"
        );
        assert!(
            row("create_issue").ends_with("mcp__github__create_issue"),
            "{frame}"
        );
        assert!(!row("create_issue").contains("read-only"), "{frame}");
        assert!(
            row("search_rep").ends_with("read-only · mcp__github__search_repositories_by_topic"),
            "{frame}"
        );
        assert!(row("search_rep").starts_with("search_reposit…"), "{frame}");
        assert!(
            row("Tools").contains("permission rules use the names on the right"),
            "{frame}"
        );
        // Every row of the panel stays inside it.
        for line in frame.lines().filter(|line| line.contains("mcp__github__")) {
            assert!(line.ends_with('│'), "{line}\n{frame}");
        }
    }

    /// At 80 columns a rule name as long as
    /// `mcp__github_enterprise__list_secret_scanning_alerts` stays whole,
    /// `read-only` mark included: the tools' own names give way, down to
    /// none at all if they must.
    #[test]
    fn details_keep_a_long_rule_name_whole_at_80_columns() {
        use crate::surface_projection::McpServerStatusView;
        let mut state = test_state();
        state.mcp_catalog = crate::surface_projection::McpCatalogView {
            servers: vec![mcp_server_view(
                "github_enterprise",
                McpServerStatusView::Connected,
            )],
            tools: vec![
                mcp_tool_view("github_enterprise", "list_secret_scanning_alerts", true),
                mcp_tool_view("github_enterprise", "get_me", false),
            ],
            prompts: Vec::new(),
        };
        state.mcp_dialog = Some(crate::types::McpDialog {
            selected: 0,
            showing_details: true,
        });

        let frame = frame_string(&mut state, 80, 30);

        let rows = panel_rows(&frame);
        assert!(
            rows.iter().any(|row| row
                .ends_with("read-only · mcp__github_enterprise__list_secret_scanning_alerts")),
            "{frame}"
        );
        assert!(
            rows.iter()
                .any(|row| row.ends_with("mcp__github_enterprise__get_me")),
            "{frame}"
        );
        for line in frame
            .lines()
            .filter(|line| line.contains("mcp__github_enterprise__"))
        {
            assert!(line.ends_with('│'), "{line}\n{frame}");
        }
        // Five columns narrower, the rule name takes every column a row has,
        // and the tools' own names none.
        let frame = frame_string(&mut state, 75, 30);
        assert!(
            panel_rows(&frame)
                .contains(&"read-only · mcp__github_enterprise__list_secret_scanning_alerts"),
            "{frame}"
        );
    }

    #[test]
    fn details_show_prompt_arguments() {
        let mut state = github_details();

        let frame = frame_string(&mut state, 80, 30);

        let rows = panel_rows(&frame);
        // One row each, its description on one line.
        assert!(
            rows.iter()
                .any(|row| row.starts_with("review_pr <pr> [branch]")
                    && row.ends_with("Review a pull request")),
            "{frame}"
        );
        // A long one keeps its arguments, before its description.
        assert!(
            rows.iter()
                .any(|row| row
                    .starts_with("summarize_discussion <discussion> [since] [until] [format]")),
            "{frame}"
        );
    }

    #[test]
    fn details_show_why_prompts_are_unavailable() {
        let mut state = github_details();
        state.mcp_catalog.prompts.clear();
        state.mcp_catalog.servers[0].prompts_error =
            Some("MCP error -32603: the prompt index is\nrebuilding".to_string());

        let frame = frame_string(&mut state, 80, 30);

        assert!(
            panel_rows(&frame)
                .contains(&"prompts unavailable: MCP error -32603: the prompt index is rebuilding"),
            "{frame}"
        );
        assert!(
            frame.contains("read-only · mcp__github__list_issues"),
            "{frame}"
        );
        // A server whose prompts were listed says nothing of the kind.
        let mut state = github_details();
        let frame = frame_string(&mut state, 80, 30);
        assert!(!frame.contains("prompts unavailable"), "{frame}");
    }
}
