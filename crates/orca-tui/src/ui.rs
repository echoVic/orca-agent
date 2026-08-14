use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use tui_textarea::TextArea;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use orca_core::approval_types::ApprovalMode;
use orca_core::task_types::{
    BackgroundTaskSummary, TaskActivitySummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
};
use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowRunStatus};
use orca_file_search::SearchPhase;
use orca_runtime::history::SessionSummary;

use crate::display_text::{compact_long_text, truncate_to_display_width};
use crate::selection::{TranscriptSelection, apply_style_to_line_range};
use crate::session_picker_actions::available_session_actions;
use crate::shortcuts::{self, ShortcutScope};
use crate::syntax_highlight::highlight_code;
use crate::theme::Theme;
use crate::transcript_search::TranscriptSearchState;
use crate::transcript_view::{TranscriptRenderContext, viewport_paragraph};
use crate::types::{
    AppState, AppStatus, ApprovalOption, ChatMessage, CopyNotice, PanelMode, SessionPickerPhase,
};
use crate::workspace_status::{GitIdentity, compact_cwd};

pub fn render(frame: &mut Frame, state: &mut AppState, textarea: &TextArea, theme: &Theme) {
    // Recomputed below when the widgets are actually shown; cleared here so
    // panel/status switches never leave stale mouse hit targets behind.
    state.jump_to_bottom_area = None;
    state.frame_area = Some(frame.area());
    state.input_area = None;
    state.search_area = None;
    if state.status == AppStatus::Setup {
        render_setup(frame, state, textarea, theme);
        return;
    }
    if state.status == AppStatus::SessionPicker {
        render_session_picker(frame, state, theme);
        return;
    }

    let search_height = u16::from(search_visible(state));
    let show_composer_hardware_cursor =
        main_composer_hardware_cursor_visible(state) && search_height == 0;
    let composer_layout = composer_visible(state)
        .then(|| composer_visual_layout(frame.area().width, textarea, theme));
    let input_height = composer_layout
        .as_ref()
        .map(|layout| composer_input_height(frame.area().width, textarea, layout))
        .unwrap_or(0);

    let plan_height = plan_panel_height(state);
    let goal_height: u16 = if state.current_goal.is_some() { 3 } else { 0 };
    // An activity indicator sits above the composer while the agent is working (or
    // waiting on the user), showing status + elapsed time. It takes two rows — a blank
    // spacer, then the text — so the transcript tail, the indicator, and the input box
    // don't sit flush against each other. Idle collapses it to zero height so a resting
    // session has no chrome noise there.
    let activity_height: u16 = if activity_line(state, theme).is_some() {
        2
    } else {
        0
    };
    let queue_preview_lines = queued_preview_lines(state, frame.area().width, theme);
    let queue_preview_height = queue_preview_lines.len().min(3) as u16;

    let chunks = main_layout(
        frame.area(),
        goal_height,
        plan_height,
        activity_height,
        queue_preview_height,
        search_height,
        input_height,
    );

    if goal_height > 0 {
        render_goal_banner(frame, chunks[0], state, theme);
    }
    let compact_conversation_background = state.status == AppStatus::WaitingApproval;
    match state.panel_mode {
        PanelMode::Conversation => render_live_messages(frame, chunks[1], state, theme),
        PanelMode::Workflows => render_workflows_panel(frame, chunks[1], state, theme),
        PanelMode::Agents => render_agents_panel(frame, chunks[1], state, theme),
    }
    let _ = compact_conversation_background;
    if plan_height > 0 {
        render_plan_panel(frame, chunks[2], state, theme);
    }
    if activity_height > 0 {
        render_activity(frame, chunks[3], state, theme);
    }
    if queue_preview_height > 0 {
        frame.render_widget(Paragraph::new(queue_preview_lines), chunks[4]);
    }
    if search_height > 0 {
        state.search_area = Some(chunks[5]);
        render_search_bar(frame, chunks[5], state, theme);
    }
    if composer_visible(state) {
        state.input_area = Some(chunks[6]);
        render_input(
            frame,
            chunks[6],
            textarea,
            composer_layout.as_ref().expect("visible composer layout"),
            state,
            theme,
            show_composer_hardware_cursor,
        );
    }
    render_status(frame, chunks[7], state, theme);

    if !state.transcript_search.open && state.slash_menu.is_some() {
        render_slash_menu(frame, chunks[6], state, theme);
    }

    if !state.transcript_search.open && state.mention.phase.is_some() && state.slash_menu.is_none()
    {
        render_mention_candidates(frame, chunks[6], state, theme);
    }

    if state.status == AppStatus::WaitingApproval {
        render_approval_dialog(frame, state, theme);
    }

    if state.status == AppStatus::Idle && state.plan_approval_dialog.is_some() {
        render_plan_approval_dialog(frame, state, theme);
    }

    if state.status == AppStatus::Idle && state.recovery_prompt_visible {
        render_recovery_prompt(frame, state, theme);
    }

    if state.show_shortcuts {
        render_shortcuts(frame, state, theme);
    }
}

fn render_recovery_prompt(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = centered_rect(frame.area(), 58, 7);
    frame.render_widget(Clear, area);
    let continue_style = if state.recovery_prompt_selected == 0 {
        Style::default()
            .fg(theme.border)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let cancel_style = if state.recovery_prompt_selected == 1 {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };
    let content = vec![
        Line::from("A suspended operation can continue from its last checkpoint."),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                if state.recovery_prompt_selected == 0 {
                    "> Continue"
                } else {
                    "  Continue"
                },
                continue_style,
            ),
            Span::raw("    "),
            Span::styled(
                if state.recovery_prompt_selected == 1 {
                    "> Cancel operation"
                } else {
                    "  Cancel operation"
                },
                cancel_style,
            ),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Recover Operation ")
        .border_style(Style::default().fg(theme.border));
    frame.render_widget(
        Paragraph::new(content)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn main_layout(
    area: Rect,
    goal_height: u16,
    plan_height: u16,
    activity_height: u16,
    queue_preview_height: u16,
    search_height: u16,
    input_height: u16,
) -> std::rc::Rc<[Rect]> {
    // The fixed chrome (goal banner, plan, activity line, input box, status line) MUST keep
    // its height so the input box stays pinned at the bottom; the message transcript takes
    // whatever is left. In ratatui 0.29 `Min` has the HIGHEST solver priority and `Fill` the
    // LOWEST, so giving the transcript `Min(5)` made it steal rows from the `Length` chrome
    // when the transcript overflowed — the input box got squeezed off-screen and the
    // auto-scrolled tail landed behind it. `Fill(1)` makes the transcript yield instead.
    let fixed_without_queue = goal_height
        .saturating_add(plan_height)
        .saturating_add(activity_height)
        .saturating_add(search_height)
        .saturating_add(input_height)
        .saturating_add(1);
    let queue_preview_height =
        queue_preview_height.min(area.height.saturating_sub(fixed_without_queue));
    Layout::vertical([
        Constraint::Length(goal_height),
        Constraint::Fill(1),
        Constraint::Length(plan_height),
        Constraint::Length(activity_height),
        Constraint::Length(queue_preview_height),
        Constraint::Length(search_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(area)
}

/// A `width`×`height` rect centered inside `area`, clamped so it never extends past
/// `area`'s bounds.
///
/// Floating popups (setup, approval dialog, shortcuts, panel overlays) are positioned by
/// centering within `frame.area()`. Under the inline viewport, `frame.area()` does NOT start
/// at `(0, 0)` — its origin is wherever the viewport is anchored (e.g. `y: 31`). Computing the
/// offset as `(area.height - height) / 2` alone yields a coordinate relative to `(0, 0)`, so
/// the popup lands *above* the viewport's buffer and `Buffer::index_of` panics with "index
/// outside of buffer". Adding `area.x`/`area.y` keeps the popup inside the actual buffer; the
/// final `clamp`/`min` guarantees it stays in bounds even when `width`/`height` exceed `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn composer_visible(state: &AppState) -> bool {
    !matches!(state.status, AppStatus::WaitingApproval) && state.plan_approval_dialog.is_none()
}

fn main_composer_hardware_cursor_visible(state: &AppState) -> bool {
    composer_visible(state) && !state.show_shortcuts
}

fn search_visible(state: &AppState) -> bool {
    state.transcript_search.open
        && state.plan_approval_dialog.is_none()
        && state.panel_mode == PanelMode::Conversation
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
}

fn queued_preview_lines(state: &AppState, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if state.panel_mode != PanelMode::Conversation
        || !matches!(state.status, AppStatus::Idle | AppStatus::Running)
        || !state.queued_follow_up_pending_or_in_flight()
        || width == 0
    {
        return Vec::new();
    }
    let Some(view) = state.queued_submission_view() else {
        return Vec::new();
    };
    let snapshot = view.preview;
    let width = width as usize;
    let header = view.error.as_ref().map_or_else(
        || format!(" Queued {} · Alt+Up edit latest", snapshot.len),
        |error| format!(" Queue error · {error}"),
    );
    let header_color = if view.error.is_some() {
        theme.error
    } else {
        theme.muted
    };
    let mut lines = vec![Line::from(Span::styled(
        truncate_to_display_width(&header, width),
        Style::default().fg(header_color),
    ))];
    let item_style = Style::default()
        .fg(theme.muted)
        .add_modifier(Modifier::ITALIC);
    lines.push(Line::from(Span::styled(
        truncate_to_display_width(&format!(" ↳ {}", snapshot.first), width),
        item_style,
    )));
    if let Some(second) = snapshot.second {
        lines.push(Line::from(Span::styled(
            truncate_to_display_width(&format!(" ↳ {second}"), width),
            item_style,
        )));
    } else if let Some(latest) = snapshot.latest {
        lines.push(Line::from(Span::styled(
            truncate_to_display_width(
                &format!(
                    " … {} more · latest: {latest}",
                    snapshot.len.saturating_sub(1)
                ),
                width,
            ),
            item_style,
        )));
    }
    lines.truncate(3);
    lines
}

fn render_goal_banner(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    use orca_core::goal_types::{
        ThreadGoalStatus, format_goal_elapsed_seconds, format_tokens_compact, goal_status_label,
    };

    let Some(goal) = &state.current_goal else {
        return;
    };

    let status_color = match goal.status {
        ThreadGoalStatus::Active => theme.success,
        ThreadGoalStatus::Paused => theme.warning,
        ThreadGoalStatus::Blocked => theme.error,
        ThreadGoalStatus::UsageLimited
        | ThreadGoalStatus::BudgetLimited
        | ThreadGoalStatus::Stalled => theme.warning,
        ThreadGoalStatus::Complete => theme.success,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" ⌖ Goal ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut metadata_spans = vec![Span::styled(
        format!("● {}", goal_status_label(goal.status)),
        Style::default().fg(status_color),
    )];
    if goal.time_used_seconds > 0 {
        metadata_spans.push(Span::styled(
            format!(
                "  · {}",
                format_goal_elapsed_seconds(goal.time_used_seconds)
            ),
            Style::default().fg(theme.muted),
        ));
    }
    if goal.tokens_used > 0 {
        metadata_spans.push(Span::styled(
            format!("  · {} tok", format_tokens_compact(goal.tokens_used)),
            Style::default().fg(theme.muted),
        ));
    }
    if goal.status.should_continue() {
        metadata_spans.push(Span::styled(
            "  · auto-continue",
            Style::default().fg(theme.muted),
        ));
    }

    let metadata_width = metadata_spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let separator_width = 2usize;
    let objective_width = (inner.width as usize)
        .saturating_sub(metadata_width)
        .saturating_sub(separator_width);
    let objective = goal
        .objective
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let objective = truncate_to_display_width(&objective, objective_width);
    let has_objective = !objective.is_empty();

    let mut spans = vec![Span::styled(
        objective,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
    if has_objective {
        spans.push(Span::raw("  "));
    }
    spans.append(&mut metadata_spans);

    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, inner);
}

fn render_session_picker(frame: &mut Frame, state: &mut AppState, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Resume Conversation ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filtered = state.filtered_session_indices();
    let loaded = state.session_picker_sessions.len();

    let mut lines = Vec::new();

    // Search field: live query + match count.
    let query_display = if state.session_picker_query.is_empty() {
        Span::styled("type to filter…", Style::default().fg(theme.muted))
    } else {
        Span::styled(
            state.session_picker_query.clone(),
            Style::default().fg(theme.text),
        )
    };
    lines.push(Line::from(vec![
        Span::styled("⌕ ", Style::default().fg(theme.border)),
        query_display,
        Span::styled(
            if state.session_picker_backfill_complete {
                format!("    {} loaded", filtered.len())
            } else {
                format!(
                    "    {}/{} loaded · indexing history…",
                    filtered.len(),
                    loaded
                )
            },
            Style::default().fg(theme.muted),
        ),
    ]));
    let hints = match state.session_picker_phase {
        SessionPickerPhase::Browsing => {
            "↑↓ select · PgUp/PgDn page · Enter resume · Tab actions · Backspace edit · Esc quit"
        }
        SessionPickerPhase::Actions { .. } => "↑↓ select action · Enter choose · Esc sessions",
        SessionPickerPhase::Renaming { .. } => "Enter rename · Esc actions",
        SessionPickerPhase::ConfirmArchive { .. } | SessionPickerPhase::ConfirmDelete { .. } => {
            "←→ choose · Enter confirm · Esc cancel"
        }
    };
    lines.push(Line::from(Span::styled(
        hints,
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No sessions match this filter.",
            Style::default().fg(theme.muted),
        )));
    }

    let needle = state.session_picker_query.to_lowercase();
    let visible_sessions = if state.session_picker_phase == SessionPickerPhase::Browsing {
        filtered.clone()
    } else {
        filtered
            .iter()
            .copied()
            .filter(|index| *index == state.session_picker_selected)
            .collect()
    };
    let mut selected_line_offset: u16 = lines.len() as u16;
    for index in visible_sessions {
        let session = &state.session_picker_sessions[index];
        let selected = index == state.session_picker_selected;
        let marker = if selected { "> " } else { "  " };
        let base = if selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        let mut spans = vec![Span::styled(marker, base)];
        // Highlight the matched substring inside the title.
        spans.extend(highlight_match(&session.title, &needle, base, theme));
        spans.push(Span::styled(
            format!(
                "  {}  {}",
                session.updated_at.format("%Y-%m-%d %H:%M"),
                session.provider
            ),
            Style::default().fg(theme.muted),
        ));
        lines.push(Line::from(spans));

        if let Some(metadata) = session_permission_metadata_label(session) {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(metadata, Style::default().fg(theme.muted)),
            ]));
        }
        if selected {
            selected_line_offset = lines.len() as u16;
        }
    }

    if let Some(error) = state.session_picker_error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error,
            Style::default().fg(theme.error),
        )));
    }

    match &state.session_picker_phase {
        SessionPickerPhase::Browsing => {}
        SessionPickerPhase::Actions {
            session_id,
            selected,
        } => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Session actions",
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )));
            for (index, action) in
                available_session_actions(state.current_session_id.as_deref(), session_id)
                    .into_iter()
                    .enumerate()
            {
                let style = if index == *selected {
                    Style::default()
                        .fg(theme.border)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} {}",
                        if index == *selected { ">" } else { " " },
                        action.label()
                    ),
                    style,
                )));
            }
        }
        SessionPickerPhase::Renaming { value, .. } => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("New title: ", Style::default().fg(theme.muted)),
                Span::styled(format!("{value}_"), Style::default().fg(theme.text)),
            ]));
        }
        SessionPickerPhase::ConfirmArchive {
            title, selected, ..
        } => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Archive \"{title}\"?"),
                Style::default().fg(theme.text),
            )));
            lines.push(confirmation_line(*selected, "Archive", theme));
        }
        SessionPickerPhase::ConfirmDelete {
            title, selected, ..
        } => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Permanently delete \"{title}\"?"),
                Style::default().fg(theme.error),
            )));
            lines.push(confirmation_line(*selected, "Delete", theme));
        }
    }

    let available_height = inner.height as u16;
    let scroll_offset = if selected_line_offset + 1 >= available_height {
        (selected_line_offset + 1).saturating_sub(available_height)
    } else {
        0
    };

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));
    frame.render_widget(paragraph, inner);
}

fn confirmation_line<'a>(selected: usize, confirm_label: &'a str, theme: &Theme) -> Line<'a> {
    let cancel = if selected == 0 {
        Style::default()
            .fg(theme.border)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let confirm = if selected == 1 {
        Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };
    Line::from(vec![
        Span::styled(
            if selected == 0 {
                "> Cancel"
            } else {
                "  Cancel"
            },
            cancel,
        ),
        Span::raw("    "),
        Span::styled(
            format!("{} {confirm_label}", if selected == 1 { ">" } else { " " }),
            confirm,
        ),
    ])
}

fn session_permission_metadata_label(session: &SessionSummary) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(profile) = &session.active_permission_profile {
        parts.push(format!("profile {}", profile.id));
    }
    if session.permission_rule_count > 0 {
        parts.push(format!(
            "{} rule{}",
            session.permission_rule_count,
            if session.permission_rule_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if !session.additional_working_directories.is_empty() {
        let labels = session
            .additional_working_directories
            .iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    entry.source,
                    workspace_relative_path_label(&entry.path, &session.runtime_workspace_roots)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("dirs {labels}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("  "))
    }
}

fn workspace_relative_path_label(path: &Path, runtime_workspace_roots: &[PathBuf]) -> String {
    let Some(root) = runtime_workspace_roots
        .iter()
        .filter(|root| path == root.as_path() || path.starts_with(root))
        .max_by_key(|root| root.components().count())
    else {
        return path.display().to_string();
    };

    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ":workspace_roots".to_string(),
        Ok(relative) => format!(":workspace_roots/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Split `text` into styled spans, highlighting the first case-insensitive
/// occurrence of `needle` with the theme warning color. Empty needle returns
/// the whole text in `base` style.
fn highlight_match(text: &str, needle: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    if needle.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_lowercase();
    let Some(start) = lower.find(needle) else {
        return vec![Span::styled(text.to_string(), base)];
    };
    let end = start + needle.len();
    let hl = base.fg(theme.warning).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base));
    }
    spans.push(Span::styled(text[start..end].to_string(), hl));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base));
    }
    spans
}

/// Render the transcript messages into `area` with no border. While `auto_scroll` is on
/// the newest content is pinned to the bottom of `area`; once the user scrolls up
/// (PageUp, k/j, etc.) `auto_scroll` clears and the pane honours `scroll_offset`.
/// Overlay the mouse selection on materialized transcript rows. The render
/// caches stay selection-agnostic so highlighting never invalidates them.
fn apply_transcript_overlays(
    mut lines: Vec<ratatui::text::Line<'static>>,
    search: &TranscriptSearchState,
    selection: Option<TranscriptSelection>,
    base_row: usize,
    theme: &Theme,
) -> Vec<ratatui::text::Line<'static>> {
    let end_row = base_row.saturating_add(lines.len());
    for (index, found) in search.visible_matches(base_row, end_row) {
        let style = if search.active_index() == Some(index) {
            theme.search_match_active_style()
        } else {
            theme.search_match_style()
        };
        for absolute_row in found.start.row..=found.last_covered_row() {
            let Some(line) = lines.get_mut(absolute_row.saturating_sub(base_row)) else {
                continue;
            };
            let Some((col_start, col_end)) = found.cols_on_row(absolute_row) else {
                continue;
            };
            let current = std::mem::take(line);
            *line = apply_style_to_line_range(current, col_start, col_end, style);
        }
    }

    let Some(selection) = selection else {
        return lines;
    };
    lines
        .into_iter()
        .enumerate()
        .map(
            |(index, line)| match selection.cols_on_row(base_row + index) {
                Some((col_start, col_end)) => {
                    apply_style_to_line_range(line, col_start, col_end, theme.selection_style())
                }
                None => line,
            },
        )
        .collect()
}

pub(crate) fn render_live_messages(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    theme: &Theme,
) {
    let width = area.width.max(1) as usize;
    let visible_height = area.height as usize;
    state.reconcile_message_tracking();
    state.transcript_area = Some(area);

    if state.messages.is_empty() {
        state
            .transcript_search
            .clear_matches(state.transcript_render_cache.content_generation());
        // The welcome screen renders through its own cache so its text is
        // selectable and copyable exactly like transcript content.
        let lines = build_welcome_lines(state, theme);
        let welcome_message = [ChatMessage::System(String::new())];
        // Sentinel revision: never collides with allocated ones, and the
        // explicit invalidate below forces a rebuild whenever we redraw.
        let welcome_revisions = [u64::MAX];
        state.welcome_render_cache.invalidate(0);
        state.welcome_render_cache.prepare(
            &welcome_message,
            &welcome_revisions,
            TranscriptRenderContext::new(theme, width, state.tick, false),
            |_, _, _, _, _, _| lines.clone(),
        );
        let requested_scroll = if state.auto_scroll {
            usize::MAX
        } else {
            state.scroll_offset
        };
        let viewport = state
            .welcome_render_cache
            .viewport(0, requested_scroll, visible_height);
        state.total_lines = viewport.total_height;
        state.visible_height = visible_height;
        state.scroll_offset = viewport.scroll_offset;
        state.viewport_base_row = viewport.base_row;
        let lines = apply_transcript_overlays(
            viewport.lines,
            &state.transcript_search,
            state.selection,
            viewport.base_row,
            theme,
        );
        frame.render_widget(viewport_paragraph(lines), area);
        return;
    }

    let mut requested_scroll = if state.auto_scroll {
        usize::MAX
    } else {
        state.scroll_offset
    };
    let live_start = state.flushed_count.min(state.messages.len());
    let messages = &state.messages;
    let revisions = &state.message_revisions;
    let highlights = state.edit_highlights.applied();
    {
        let cache = &mut state.transcript_render_cache;
        let outcome = cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(theme, width, state.tick, false).with_reflow_window(
                live_start,
                requested_scroll,
                visible_height,
            ),
            |index, message, theme, width, tick, force_expand| {
                let refined = AppState::refined_diff_styles_for_message(
                    revisions, highlights, index, message,
                );
                build_lines_for_message(message, theme, width, tick, force_expand, refined)
            },
        );
        requested_scroll = outcome.adjusted_scroll.unwrap_or(requested_scroll);
    }
    state.refresh_transcript_search();
    let viewport =
        state
            .transcript_render_cache
            .viewport(live_start, requested_scroll, visible_height);
    state.total_lines = viewport.total_height;
    state.visible_height = visible_height;
    state.scroll_offset = viewport.scroll_offset;
    state.viewport_base_row = viewport.base_row;

    // Overlay the mouse selection on the materialized rows; the render cache
    // itself stays selection-agnostic so highlighting never invalidates it.
    let lines = apply_transcript_overlays(
        viewport.lines,
        &state.transcript_search,
        state.selection,
        viewport.base_row,
        theme,
    );

    frame.render_widget(viewport_paragraph(lines), area);

    // Floating "jump to bottom" pill, shown while the user has scrolled away
    // from the tail (auto-follow disarmed). While detached it doubles as an
    // unread indicator: messages landing below bump `unseen_messages`.
    // Clicking it re-arms follow and clears the count.
    if !state.auto_scroll && viewport.total_height > visible_height && area.height > 0 {
        let label = match state.unseen_messages {
            0 => " Jump to bottom (click) ↓ ".to_string(),
            1 => " 1 new message (click) ↓ ".to_string(),
            count => format!(" {count} new messages (click) ↓ "),
        };
        let pill_width = UnicodeWidthStr::width(label.as_str()) as u16;
        if area.width >= pill_width {
            let pill = Rect {
                x: area.x + (area.width - pill_width) / 2,
                y: area.y + area.height - 1,
                width: pill_width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(label, theme.selection_style().fg(theme.text))),
                pill,
            );
            state.jump_to_bottom_area = Some(pill);
        }
    }
}

fn render_workflows_panel(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Tasks ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tasks = state.workflow_panel.tasks.iter().collect::<Vec<_>>();

    if tasks.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                " No background tasks available in this view yet.",
                Style::default().fg(theme.muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    // One hint row + one header row + task rows. The selected workflow expands into
    // phase and per-agent rows so the panel can act as a lightweight dashboard.
    let hint_h: u16 = 1;
    let header_h: u16 = 1;
    let row_h: u16 = 2;
    let mut constraints = vec![Constraint::Length(hint_h), Constraint::Length(header_h)];
    constraints.extend(tasks.iter().enumerate().map(|(index, task)| {
        let detail_rows = if index == state.workflow_panel.selected {
            workflow_metadata_row_count(task)
                + workflow_phase_detail_rows(task).len() as u16
                + workflow_agent_row_count(task)
        } else {
            0
        };
        Constraint::Length(row_h.saturating_add(detail_rows))
    }));
    constraints.push(Constraint::Min(0));
    let rows = Layout::vertical(constraints).split(inner);

    let selected_task = state
        .workflow_panel
        .tasks
        .get(state.workflow_panel.selected);
    frame.render_widget(
        Paragraph::new(workflow_panel_action_hint(selected_task, theme)),
        rows[0],
    );

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Name", Style::default().fg(theme.muted)),
        Span::styled("   Type", Style::default().fg(theme.muted)),
        Span::styled("       Status", Style::default().fg(theme.muted)),
        Span::styled("      Detail", Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(header, rows[1]);

    for (index, task) in tasks.iter().enumerate() {
        let row_area = rows[index + 2];
        let selected = index == state.workflow_panel.selected;
        let marker = if selected { ">" } else { " " };
        let name = task.name.as_deref().unwrap_or(task.description.as_str());
        let task_type = task_type_label(task);
        let status = task_status_label(task.status);
        let status_color = task_status_color(task.status, theme);
        let detail = task_detail_label(task);
        let name_style = if selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        // Split the row into a label line, a gauge line, and optional agent rows.
        let mut row_constraints = vec![Constraint::Length(1), Constraint::Length(1)];
        let metadata_rows = if selected {
            workflow_metadata_rows(task, theme)
        } else {
            Vec::new()
        };
        if selected {
            row_constraints.extend(metadata_rows.iter().map(|_| Constraint::Length(1)));
            row_constraints.extend(
                workflow_phase_detail_rows(task)
                    .iter()
                    .map(|_| Constraint::Length(1)),
            );
            for agent in &task.workflow_agents {
                row_constraints.push(Constraint::Length(1));
                if agent.transcript_path.is_some() {
                    row_constraints.push(Constraint::Length(1));
                }
            }
        }
        let parts = Layout::vertical(row_constraints).split(row_area);

        let label = Paragraph::new(Line::from(vec![
            Span::styled(format!("{marker} {name}"), name_style),
            Span::styled("  ", Style::default()),
            Span::styled(task_type, Style::default().fg(theme.muted)),
            Span::styled("  ", Style::default()),
            Span::styled(status.to_string(), Style::default().fg(status_color)),
            Span::styled(format!("  {detail}"), Style::default().fg(theme.muted)),
        ]));
        frame.render_widget(label, parts[0]);

        // Gauge ratio reflects lifecycle, not fabricated progress: terminal
        // states fill the bar, queued/paused stay empty, and a running task
        // shows a tick-driven activity pulse. The status word stays in the
        // label so a moving bar can't be misread as a real percentage.
        let ratio = workflow_gauge_ratio(task.status, state.tick);
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(status_color).bg(theme.muted))
            .ratio(ratio)
            .label(Span::styled(
                workflow_gauge_label(task.status),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(gauge, parts[1]);

        if selected {
            let phase_rows = workflow_phase_detail_rows(task);
            for (metadata_index, line) in metadata_rows.iter().enumerate() {
                frame.render_widget(Paragraph::new(line.clone()), parts[metadata_index + 2]);
            }
            let detail_offset = metadata_rows.len() + 2;
            for (phase_index, phase) in phase_rows.iter().enumerate() {
                let line = Paragraph::new(workflow_phase_row_label(phase, theme));
                frame.render_widget(line, parts[detail_offset + phase_index]);
            }
            let mut agent_row_index = detail_offset + phase_rows.len();
            for agent in &task.workflow_agents {
                let line = Paragraph::new(agent_row_label(agent, theme));
                frame.render_widget(line, parts[agent_row_index]);
                agent_row_index += 1;
                if let Some(path) = agent.transcript_path.as_deref() {
                    let line = Paragraph::new(agent_transcript_row_label(path, theme));
                    frame.render_widget(line, parts[agent_row_index]);
                    agent_row_index += 1;
                }
            }
        }
    }
}

fn workflow_panel_action_hint<'a>(
    selected_task: Option<&BackgroundTaskSummary>,
    theme: &Theme,
) -> Line<'a> {
    let mut spans = vec![Span::styled(" ↑↓ select", Style::default().fg(theme.muted))];
    if selected_task.is_some_and(is_approval_actionable_task) {
        spans.push(Span::styled(
            " · Enter approve",
            Style::default().fg(theme.muted),
        ));
    }
    if selected_task.is_some_and(is_stoppable_task) {
        spans.push(Span::styled(" · s stop", Style::default().fg(theme.muted)));
    }
    if selected_task.is_some_and(is_foregroundable_task) {
        spans.push(Span::styled(
            " · f foreground",
            Style::default().fg(theme.muted),
        ));
    }
    spans.push(Span::styled(
        " · Esc close",
        Style::default().fg(theme.muted),
    ));
    Line::from(spans)
}

fn is_approval_actionable_task(task: &BackgroundTaskSummary) -> bool {
    task.task_type == TaskType::MainSession
        && task.status == TaskStatus::ApprovalRequired
        && task.is_backgrounded
        && task.pending_tool_call.is_some()
}

fn is_stoppable_task(task: &BackgroundTaskSummary) -> bool {
    !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Stopped
    )
}

fn is_foregroundable_task(task: &BackgroundTaskSummary) -> bool {
    task.task_type == TaskType::MainSession
        && task.status == TaskStatus::Running
        && task.is_backgrounded
}

fn render_agents_panel(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Agents ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = state
        .workflow_panel
        .tasks
        .iter()
        .flat_map(|task| {
            let workflow_name = task.name.as_deref().unwrap_or(task.description.as_str());
            task.workflow_agents
                .iter()
                .map(move |agent| (workflow_name, agent))
        })
        .collect::<Vec<_>>();

    if rows.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                " No workflow agents available yet.",
                Style::default().fg(theme.muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    let mut constraints = vec![Constraint::Length(1)];
    constraints.extend(rows.iter().map(|_| Constraint::Length(1)));
    constraints.push(Constraint::Min(0));
    let areas = Layout::vertical(constraints).split(inner);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Workflow", Style::default().fg(theme.muted)),
        Span::styled("   Agent", Style::default().fg(theme.muted)),
        Span::styled("      Status", Style::default().fg(theme.muted)),
        Span::styled("      Detail", Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(header, areas[0]);

    for (index, (workflow_name, agent)) in rows.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(agent_dashboard_row_label(workflow_name, agent, theme)),
            areas[index + 1],
        );
    }
}

fn workflow_phase_detail_rows(
    task: &BackgroundTaskSummary,
) -> Vec<&orca_core::task_types::WorkflowPhaseTaskSummary> {
    task.workflow_phases
        .iter()
        .filter(|phase| {
            phase.error.is_some()
                || phase.fallback.is_some()
                || matches!(
                    phase.status,
                    WorkflowRunStatus::Failed
                        | WorkflowRunStatus::Cancelled
                        | WorkflowRunStatus::Stopped
                )
        })
        .collect()
}

const TASK_DETAIL_MAX_LINES: usize = 3;
const TASK_DETAIL_MAX_CHARS: usize = 120;

fn workflow_metadata_row_count(task: &BackgroundTaskSummary) -> u16 {
    u16::from(task.workflow_run_id.is_some())
        + u16::from(task.workflow_script_path.is_some())
        + u16::from(task.workflow_launch_input.is_some())
        + u16::from(task.workflow_failure_count > 0)
        + u16::from(task.workflow_final_summary.is_some())
        + task
            .result
            .as_deref()
            .map(task_detail_line_count)
            .unwrap_or_default() as u16
        + task
            .error
            .as_deref()
            .map(task_detail_line_count)
            .unwrap_or_default() as u16
}

fn workflow_agent_row_count(task: &BackgroundTaskSummary) -> u16 {
    task.workflow_agents
        .iter()
        .map(|agent| 1 + u16::from(agent.transcript_path.is_some()))
        .sum()
}

fn workflow_metadata_rows<'a>(task: &BackgroundTaskSummary, theme: &Theme) -> Vec<Line<'a>> {
    let mut rows = Vec::new();
    if let Some(run_id) = &task.workflow_run_id {
        rows.push(Line::from(vec![
            Span::styled("    run ", Style::default().fg(theme.muted)),
            Span::styled(run_id.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(script_path) = &task.workflow_script_path {
        rows.push(Line::from(vec![
            Span::styled("    script ", Style::default().fg(theme.muted)),
            Span::styled(script_path.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(launch_input) = &task.workflow_launch_input {
        rows.push(Line::from(vec![
            Span::styled("    launch ", Style::default().fg(theme.muted)),
            Span::styled(
                workflow_launch_input_label(launch_input),
                Style::default().fg(theme.text),
            ),
        ]));
    }
    if task.workflow_failure_count > 0 {
        rows.push(Line::from(vec![
            Span::styled("    failures ", Style::default().fg(theme.muted)),
            Span::styled(
                task.workflow_failure_count.to_string(),
                Style::default().fg(theme.error),
            ),
        ]));
    }
    if let Some(summary) = &task.workflow_final_summary {
        rows.push(Line::from(vec![
            Span::styled("    final ", Style::default().fg(theme.muted)),
            Span::styled(summary.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(result) = &task.result {
        rows.extend(task_detail_text_rows(
            "result",
            result,
            Style::default().fg(theme.success),
            Style::default().fg(theme.text),
        ));
    }
    if let Some(error) = &task.error {
        rows.extend(task_detail_text_rows(
            "error",
            error,
            Style::default().fg(theme.error),
            Style::default().fg(theme.error),
        ));
    }
    rows
}

fn task_detail_line_count(text: &str) -> usize {
    text.lines().count().max(1).min(TASK_DETAIL_MAX_LINES)
}

fn task_detail_text_rows<'a>(
    label: &str,
    text: &str,
    label_style: Style,
    text_style: Style,
) -> Vec<Line<'a>> {
    let mut lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("");
    }
    let truncated = lines.len() > TASK_DETAIL_MAX_LINES;
    lines.truncate(TASK_DETAIL_MAX_LINES);

    let prefix = format!("    {label} ");
    let continuation_prefix = " ".repeat(prefix.chars().count());
    let last_index = lines.len().saturating_sub(1);

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix_text = if index == 0 {
                prefix.clone()
            } else {
                continuation_prefix.clone()
            };
            let mut detail = clamp_label(line, TASK_DETAIL_MAX_CHARS);
            if truncated && index == last_index && !detail.ends_with('…') {
                detail.push('…');
            }
            Line::from(vec![
                Span::styled(prefix_text, label_style),
                Span::styled(detail, text_style),
            ])
        })
        .collect()
}

fn workflow_launch_input_label(input: &orca_core::workflow_types::WorkflowInput) -> String {
    let mut parts = Vec::new();
    if let Some(draft_id) = input.draft_id.as_deref() {
        parts.push(format!("draftId={draft_id}"));
    }
    if let Some(name) = input.name.as_deref() {
        parts.push(format!("name={name}"));
    }
    if let Some(script_path) = input.script_path.as_deref() {
        parts.push(format!("scriptPath={script_path}"));
    }
    if let Some(resume_from) = input.resume_from_run_id.as_deref() {
        parts.push(format!("resumeFrom={resume_from}"));
    }
    if let Some(args) = &input.args {
        parts.push(format!("args={args}"));
    }
    if parts.is_empty() {
        "inline script".to_string()
    } else {
        parts.join(" ")
    }
}

/// Truthful gauge fill for a workflow lifecycle state.
///
/// We don't have a completed-phase count in the task model, so we never
/// invent a percentage. Terminal states fill the bar; queued/paused are
/// empty; running animates a bounded pulse from the UI tick.
fn workflow_gauge_ratio(status: TaskStatus, tick: u64) -> f64 {
    match status {
        TaskStatus::Completed => 1.0,
        TaskStatus::Failed | TaskStatus::Cancelled => 1.0,
        TaskStatus::Queued
        | TaskStatus::Paused
        | TaskStatus::Stopped
        | TaskStatus::ApprovalRequired => 0.0,
        TaskStatus::Running | TaskStatus::Stopping => {
            // Triangle wave in [0.15, 0.85] so the bar visibly breathes.
            let period = 20u64;
            let phase = (tick % period) as f64 / period as f64;
            let tri = if phase < 0.5 {
                phase * 2.0
            } else {
                2.0 - phase * 2.0
            };
            0.15 + tri * 0.7
        }
    }
}

fn workflow_gauge_label(status: TaskStatus) -> String {
    match status {
        TaskStatus::Completed => "done".to_string(),
        TaskStatus::Failed => "failed".to_string(),
        TaskStatus::ApprovalRequired => "approval required".to_string(),
        TaskStatus::Cancelled => "cancelled".to_string(),
        TaskStatus::Queued => "queued".to_string(),
        TaskStatus::Paused => "paused".to_string(),
        TaskStatus::Stopped => "stopped".to_string(),
        TaskStatus::Running => "running…".to_string(),
        TaskStatus::Stopping => "stopping…".to_string(),
    }
}

fn task_type_label(task: &BackgroundTaskSummary) -> &'static str {
    match task.task_type {
        TaskType::MainSession => "session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn task_detail_label(task: &BackgroundTaskSummary) -> String {
    let detail = match task.task_type {
        TaskType::Workflow => workflow_progress_label(task),
        TaskType::Subagent => subagent_progress_label(task),
        TaskType::MainSession
            if task.is_backgrounded && task.status == TaskStatus::ApprovalRequired =>
        {
            if let Some(tool) = task.tool.as_deref() {
                return format!("waiting on {tool} • backgrounded • {}", elapsed_label(task));
            }
            format!("backgrounded • {}", elapsed_label(task))
        }
        TaskType::MainSession if task.is_backgrounded => {
            format!("backgrounded • {}", elapsed_label(task))
        }
        TaskType::MainSession | TaskType::Shell | TaskType::Monitor => elapsed_label(task),
    };

    let mut visibility = Vec::new();
    if task.retry_count > 0 {
        visibility.push(format!("retried {}", task.retry_count));
    }
    if task.output_truncated {
        visibility.push("output truncated".to_string());
    }
    if visibility.is_empty() {
        detail
    } else {
        format!("{detail} • {}", visibility.join(" • "))
    }
}

fn workflow_progress_label(task: &BackgroundTaskSummary) -> String {
    let total_phases = task.phase_count.unwrap_or_default();
    let Some(progress) = task.workflow_progress else {
        return match task.phase_count {
            Some(count) => format!("{count} phases"),
            None => "phases -".to_string(),
        };
    };

    let mut parts = vec![format!(
        "agents {}/{}",
        progress.completed_agents, progress.total_agents
    )];
    if progress.running_agents > 0 {
        parts.push(format!("running {}", progress.running_agents));
    }
    if progress.failed_agents > 0 {
        parts.push(format!("failed {}", progress.failed_agents));
    }

    let phase_total = if total_phases == 0 {
        progress
            .completed_phases
            .saturating_add(progress.running_phases)
            .saturating_add(progress.failed_phases)
    } else {
        total_phases
    };
    parts.push(format!(
        "phases {}/{}",
        progress.completed_phases, phase_total
    ));
    parts.join(", ")
}

fn agent_row_label<'a>(agent: &WorkflowAgentTaskSummary, theme: &Theme) -> Line<'a> {
    let status = workflow_agent_status_label(agent.status);
    let status_color = workflow_agent_status_color(agent.status, theme);
    let attempt = format!("attempt {}/{}", agent.attempt, agent.max_attempts);
    let retry = if agent.previous_errors.is_empty() {
        "retry errors 0".to_string()
    } else {
        format!("retry errors {}", agent.previous_errors.len())
    };
    let team = agent
        .team
        .as_deref()
        .map(|team| format!("  team {team}"))
        .unwrap_or_default();
    let elapsed = agent_elapsed_label(agent)
        .map(|elapsed| format!("  {elapsed}"))
        .unwrap_or_default();
    let usage = agent
        .usage
        .map(|usage| {
            format!(
                "  {} tok ${:.6}",
                usage.total_tokens(),
                usage.estimated_cost_usd
            )
        })
        .unwrap_or_default();
    let error = agent
        .error
        .as_deref()
        .or_else(|| agent.previous_errors.last().map(String::as_str));
    let detail = error.map(|error| format!("  {error}")).unwrap_or_default();

    Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(agent.call_path.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(team, Style::default().fg(theme.muted)),
        Span::styled(format!("  {attempt}"), Style::default().fg(theme.muted)),
        Span::styled(format!("  {retry}"), Style::default().fg(theme.muted)),
        Span::styled(elapsed, Style::default().fg(theme.muted)),
        Span::styled(usage, Style::default().fg(theme.muted)),
        Span::styled(detail, Style::default().fg(theme.error)),
    ])
}

fn agent_transcript_row_label<'a>(path: &str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled("      full result ", Style::default().fg(theme.muted)),
        Span::styled(path.to_string(), Style::default().fg(theme.text)),
    ])
}

fn agent_dashboard_row_label<'a>(
    workflow_name: &str,
    agent: &WorkflowAgentTaskSummary,
    theme: &Theme,
) -> Line<'a> {
    let status = workflow_agent_status_label(agent.status);
    let status_color = workflow_agent_status_color(agent.status, theme);
    let attempt = format!("attempt {}/{}", agent.attempt, agent.max_attempts);
    let team = agent
        .team
        .as_deref()
        .map(|team| format!("  team {team}"))
        .unwrap_or_default();
    let elapsed = agent_elapsed_label(agent)
        .map(|elapsed| format!("  {elapsed}"))
        .unwrap_or_default();
    let usage = agent
        .usage
        .map(|usage| {
            format!(
                "  {} tok ${:.6}",
                usage.total_tokens(),
                usage.estimated_cost_usd
            )
        })
        .unwrap_or_default();
    let retry = if agent.previous_errors.is_empty() {
        String::new()
    } else {
        format!("  retry errors {}", agent.previous_errors.len())
    };
    let error = agent
        .error
        .as_deref()
        .or_else(|| agent.previous_errors.last().map(String::as_str))
        .map(|error| format!("  {error}"))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(workflow_name.to_string(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(agent.call_path.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(team, Style::default().fg(theme.muted)),
        Span::styled(format!("  {attempt}"), Style::default().fg(theme.muted)),
        Span::styled(elapsed, Style::default().fg(theme.muted)),
        Span::styled(usage, Style::default().fg(theme.muted)),
        Span::styled(retry, Style::default().fg(theme.muted)),
        Span::styled(error, Style::default().fg(theme.error)),
    ])
}

fn workflow_phase_row_label<'a>(
    phase: &orca_core::task_types::WorkflowPhaseTaskSummary,
    theme: &Theme,
) -> Line<'a> {
    let status = task_status_from_workflow_status(phase.status);
    let status_color = task_status_color(status, theme);
    let fallback = phase
        .fallback
        .as_deref()
        .map(|fallback| format!("  fallback {fallback}"))
        .unwrap_or_default();
    let error = phase
        .error
        .as_deref()
        .map(|error| format!("  {error}"))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled("    phase ", Style::default().fg(theme.muted)),
        Span::styled(phase.name.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(
            workflow_run_status_label(phase.status),
            Style::default().fg(status_color),
        ),
        Span::styled(
            format!("  agents {}", phase.agent_count),
            Style::default().fg(theme.muted),
        ),
        Span::styled(fallback, Style::default().fg(theme.muted)),
        Span::styled(error, Style::default().fg(theme.error)),
    ])
}

fn workflow_run_status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Queued => "queued",
        WorkflowRunStatus::Running => "running",
        WorkflowRunStatus::AsyncLaunched => "async",
        WorkflowRunStatus::Paused => "paused",
        WorkflowRunStatus::Stopping => "stopping",
        WorkflowRunStatus::Stopped => "stopped",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
    }
}

fn task_status_from_workflow_status(status: WorkflowRunStatus) -> TaskStatus {
    match status {
        WorkflowRunStatus::Queued => TaskStatus::Queued,
        WorkflowRunStatus::Running | WorkflowRunStatus::AsyncLaunched => TaskStatus::Running,
        WorkflowRunStatus::Paused => TaskStatus::Paused,
        WorkflowRunStatus::Stopping => TaskStatus::Stopping,
        WorkflowRunStatus::Stopped => TaskStatus::Stopped,
        WorkflowRunStatus::Completed => TaskStatus::Completed,
        WorkflowRunStatus::Failed => TaskStatus::Failed,
        WorkflowRunStatus::Cancelled => TaskStatus::Cancelled,
    }
}

fn workflow_agent_status_label(status: WorkflowAgentStatus) -> &'static str {
    match status {
        WorkflowAgentStatus::Pending => "pending",
        WorkflowAgentStatus::Running => "running",
        WorkflowAgentStatus::Cached => "cached",
        WorkflowAgentStatus::Completed => "completed",
        WorkflowAgentStatus::Failed => "failed",
        WorkflowAgentStatus::Cancelled => "cancelled",
    }
}

fn workflow_agent_status_color(status: WorkflowAgentStatus, theme: &Theme) -> Color {
    match status {
        WorkflowAgentStatus::Completed | WorkflowAgentStatus::Cached => theme.success,
        WorkflowAgentStatus::Failed | WorkflowAgentStatus::Cancelled => theme.error,
        WorkflowAgentStatus::Running => theme.warning,
        WorkflowAgentStatus::Pending => theme.muted,
    }
}

fn subagent_progress_label(task: &BackgroundTaskSummary) -> String {
    let mut parts = Vec::new();
    if let Some(agent_type) = task.agent_type.as_deref() {
        parts.push(agent_type.to_string());
    }
    if let Some(turn) = task.subagent_turn {
        parts.push(format!("turn {turn}"));
    }
    parts.push(elapsed_label(task));
    if let Some(usage) = task.usage {
        parts.push(format!(
            "{} tok ${:.6}",
            usage.total_tokens(),
            usage.estimated_cost_usd
        ));
    }
    // The activity carries a tool target of arbitrary length (often a full
    // shell command), so it is clamped and rendered last: when the row
    // truncates, the fixed-width fields stay visible.
    if let Some(activity) = task.subagent_current_activity.as_deref() {
        parts.push(clamp_label(activity, 32));
    }
    parts.join(", ")
}

fn clamp_label(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let clamped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{clamped}…")
}

fn elapsed_label(task: &BackgroundTaskSummary) -> String {
    let Some(started_at_ms) = task.started_at_ms else {
        return "not started".to_string();
    };
    let end_ms = task.completed_at_ms.unwrap_or_else(current_time_ms);
    let elapsed_ms = end_ms.saturating_sub(started_at_ms);
    format!(
        "elapsed {}",
        format_elapsed_compact((elapsed_ms / 1000) as u64)
    )
}

fn agent_elapsed_label(agent: &WorkflowAgentTaskSummary) -> Option<String> {
    let started_at_ms = agent.started_at_ms?;
    let end_ms = agent.completed_at_ms.unwrap_or_else(current_time_ms);
    let elapsed_ms = end_ms.saturating_sub(started_at_ms);
    Some(format!(
        "elapsed {}",
        format_elapsed_compact((elapsed_ms / 1000) as u64)
    ))
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn build_welcome_lines<'a>(state: &AppState, theme: &Theme) -> Vec<Line<'a>> {
    let cyan = Style::default().fg(theme.border);
    let text = Style::default().fg(theme.text);
    let muted = Style::default().fg(theme.muted);

    vec![
        Line::from(""),
        Line::from(Span::styled("   ___                ", cyan)),
        Line::from(Span::styled("  / _ \\ _ __ ___ __ _ ", cyan)),
        Line::from(Span::styled(" | | | | '__/ __/ _` |", cyan)),
        Line::from(Span::styled(" | |_| | | | (_| (_| |", cyan)),
        Line::from(vec![
            Span::styled("  \\___/|_|  \\___\\__,_|", cyan),
            Span::styled(format!("  v{}", state.app_version), muted),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  model:      ", muted),
            Span::styled(state.model_name.clone(), text),
        ]),
        Line::from(vec![
            Span::styled("  directory:  ", muted),
            Span::styled(state.cwd.clone(), text),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Tips", Style::default().fg(theme.success))),
        Line::from(Span::styled(
            "  • Enter to send, Alt+Enter (or Shift+Enter) for newline",
            muted,
        )),
        Line::from(Span::styled(
            "  • / commands, @ to mention files, $ to invoke skills",
            muted,
        )),
        Line::from(Span::styled(
            "  • /model to switch model, /compact to compress context",
            muted,
        )),
        Line::from(Span::styled(
            "  • Ctrl+K or F1 for keyboard shortcuts",
            muted,
        )),
        Line::from(""),
    ]
}

/// Render the lines for a contiguous slice of messages. Used both to flush a settled
/// prefix into the terminal scrollback and to draw the live bottom pane, so the two
/// surfaces stay pixel-identical.
///
/// `force_expand` overrides each tool/subagent's collapsed view and renders its full
/// output. The flush path sets this so a completed tool's output is committed to the
/// immutable scrollback in full — once flushed it can never be re-expanded, so we must
/// not freeze a truncated view. The live pane passes `false` and honours the per-message
/// `expanded` flag that `e` toggles.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_lines_for_messages(
    messages: &[ChatMessage],
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for msg in messages {
        lines.extend(build_lines_for_message(
            msg,
            theme,
            width,
            tick,
            force_expand,
            None,
        ));
    }
    lines
}

pub(crate) fn build_lines_for_message(
    message: &ChatMessage,
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
    refined_diff: Option<&crate::diff_highlight::RefinedDiffStyles>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_message_lines(
        &mut lines,
        message,
        theme,
        width,
        tick,
        force_expand,
        refined_diff,
    );
    lines
}

/// Append the rendered lines for a single chat message. Pure with respect to global
/// state: the only dynamic input is `tick`, which drives the running-tool spinner.
fn append_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
    refined_diff: Option<&crate::diff_highlight::RefinedDiffStyles>,
) {
    match msg {
        ChatMessage::User(text) => {
            let text = compact_long_text(text, width.saturating_sub(2).max(1), 3);
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(theme.user)),
                Span::styled(text, Style::default().fg(theme.user)),
            ]));
            lines.push(Line::from(""));
        }
        ChatMessage::Reasoning(text) => {
            let prefix = Span::styled(
                "[thinking] ",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            );
            let truncated = truncate_lines(text, 3);
            lines.push(Line::from(vec![
                prefix,
                Span::styled(
                    truncated,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        ChatMessage::Assistant(text) => {
            append_assistant_markdown(lines, text, width, theme, true);
        }
        ChatMessage::AssistantChunk {
            text,
            trailing_blank,
        } => {
            append_assistant_markdown(lines, text, width, theme, *trailing_blank);
        }
        ChatMessage::ProposedPlan(text) => {
            append_proposed_plan_lines(lines, text, width, theme);
        }
        ChatMessage::ToolCall {
            name,
            target,
            status,
            output,
            diff,
            kind,
            expanded,
            ..
        } => {
            let neutral_completed =
                status == "completed" && matches!(kind.as_deref(), Some("empty" | "no_matches"));
            let icon = match status.as_str() {
                "completed" => "✓",
                "running" | "receiving" => spinner_frame(tick),
                "denied" => "✗",
                "failed" => "✗",
                "cancelled" => "×",
                "indeterminate" => "?",
                _ => "·",
            };
            let color = match status.as_str() {
                "completed" if neutral_completed => theme.muted,
                "completed" => theme.success,
                "running" | "receiving" => theme.warning,
                "denied" | "failed" => theme.error,
                "cancelled" | "indeterminate" => theme.warning,
                _ => theme.muted,
            };
            let display_status = match status.as_str() {
                "cancelled" => "interrupted",
                "indeterminate" => "state unknown",
                status => status,
            };
            let prefix = format!("  {icon} {name}");
            let status_text = format!(" ({display_status})");
            let reserved_width = UnicodeWidthStr::width(prefix.as_str())
                + UnicodeWidthStr::width(status_text.as_str());
            let target_width =
                width.saturating_sub(reserved_width + 2 * usize::from(target.is_some()));
            let target_str = target
                .as_deref()
                .map(|target| format!(": {}", truncate_to_display_width(target, target_width)))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}{target_str}"), Style::default().fg(color)),
                Span::styled(status_text, Style::default().fg(theme.muted)),
            ]));
            if let Some(out) = output {
                append_tool_output_lines(lines, out, *expanded, force_expand, theme);
            }
            if let Some(diff) = diff {
                append_diff_lines(lines, diff, theme, refined_diff);
            }
        }
        ChatMessage::PlanUpdate { explanation, plan } => {
            append_archived_plan_lines(lines, explanation.as_deref(), plan, width, theme);
        }
        ChatMessage::Subagent {
            description,
            status,
            output,
            error,
            activity,
            activity_tail,
            turn,
            usage,
            expanded,
            ..
        } => {
            append_subagent_lines(
                lines,
                description,
                status,
                output,
                error,
                activity.as_deref(),
                activity_tail,
                *turn,
                *usage,
                theme,
                *expanded,
                force_expand,
            );
        }
        ChatMessage::Error(text) => {
            lines.push(Line::from(Span::styled(
                format!("ERROR: {text}"),
                Style::default().fg(theme.error),
            )));
            lines.push(Line::from(""));
        }
        ChatMessage::System(text) => {
            lines.push(Line::from(Span::styled(
                text.clone(),
                Style::default().fg(theme.muted),
            )));
            lines.push(Line::from(""));
        }
    }
}

fn append_assistant_markdown(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    theme: &Theme,
    trailing_blank: bool,
) {
    lines.extend(render_markdown(text, width, theme));
    if trailing_blank {
        lines.push(Line::from(""));
    }
}

fn plan_panel_height(state: &AppState) -> u16 {
    match &state.current_plan {
        Some((_, plan)) => {
            let items = plan.len() as u16;
            // 2 for border, 1 for title = items + 2, capped at 10
            (items + 2).min(10)
        }
        None => 0,
    }
}

fn render_plan_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    use orca_core::plan_types::PlanStatus;

    let Some((_, plan)) = &state.current_plan else {
        return;
    };

    let (title, border_color) = if state.plan_update_failed {
        (
            " Task Plan (last update failed — may be stale) ",
            theme.warning,
        )
    } else {
        (" Task Plan ", theme.border)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    let step_width = (inner.width as usize).saturating_sub(3);
    for item in plan {
        let (icon, color) = match item.status {
            PlanStatus::Completed => ("✓", theme.success),
            PlanStatus::InProgress => ("→", theme.warning),
            PlanStatus::Pending => ("•", theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(color)),
            Span::styled(
                truncate_to_display_width(&item.step.replace('\n', " "), step_width),
                Style::default().fg(color),
            ),
        ]));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Render a finished plan as an inline checklist in the scrollback. Completed steps are dimmed and
/// struck through; the in-progress/pending steps keep their live-panel styling so the archived view
/// matches what the user saw in the bottom panel.
fn append_archived_plan_lines(
    lines: &mut Vec<Line<'static>>,
    explanation: Option<&str>,
    plan: &[orca_core::plan_types::PlanItem],
    width: usize,
    theme: &Theme,
) {
    use orca_core::plan_types::PlanStatus;

    lines.push(Line::from(Span::styled(
        "  Task Plan",
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    )));

    if let Some(note) = explanation.map(str::trim).filter(|n| !n.is_empty()) {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    for item in plan {
        let (icon, text_style) = match item.status {
            PlanStatus::Completed => (
                "✓",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            PlanStatus::InProgress => ("→", Style::default().fg(theme.warning)),
            PlanStatus::Pending => ("•", Style::default().fg(theme.muted)),
        };
        let icon_style = match item.status {
            PlanStatus::Completed => Style::default().fg(theme.success),
            PlanStatus::InProgress => Style::default().fg(theme.warning),
            PlanStatus::Pending => Style::default().fg(theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {icon} "), icon_style),
            Span::styled(
                truncate_to_display_width(&item.step.replace('\n', " "), width.saturating_sub(4)),
                text_style,
            ),
        ]));
    }

    lines.push(Line::from(""));
}

fn append_proposed_plan_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(vec![Span::styled(
        "  Proposed Plan",
        Style::default()
            .fg(theme.approval)
            .add_modifier(Modifier::BOLD),
    )]));
    for mut line in render_markdown(text, width.saturating_sub(2), theme) {
        line.spans
            .insert(0, Span::styled("  ", Style::default().fg(theme.muted)));
        lines.push(line);
    }
    lines.push(Line::from(""));
}

fn append_subagent_lines(
    lines: &mut Vec<Line<'static>>,
    description: &str,
    status: &str,
    output: &Option<String>,
    error: &Option<String>,
    activity: Option<&str>,
    activity_tail: &[String],
    turn: Option<u32>,
    usage: Option<orca_core::cost_types::UsageTotals>,
    theme: &Theme,
    expanded: bool,
    force_expand: bool,
) {
    let (label, color) = match status {
        "success" | "completed" => ("done", theme.success),
        "running" => ("running", theme.border),
        "failed" => ("failed", theme.error),
        other => (other, theme.muted),
    };

    lines.push(Line::from(vec![
        Span::styled("  ┌─ delegated task", Style::default().fg(theme.border)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(label.to_string(), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(theme.border)),
        Span::styled(description.to_string(), Style::default().fg(theme.text)),
    ]));

    // The collapsed view keeps only the first few lines; when flushing to the immutable
    // scrollback (`force_expand`) we emit the whole result/error so nothing is truncated
    // beyond reach.
    let body_limit = if force_expand { usize::MAX } else { 3 };
    match (status, output, error) {
        ("running", _, _) => {
            let mut detail = activity.unwrap_or("working in a child context").to_string();
            if let Some(turn) = turn {
                detail = format!("turn {turn} · {detail}");
            }
            if let Some(usage) = usage {
                detail.push_str(&format!(" · {} tok", usage.total_tokens()));
            }
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(theme.border)),
                Span::styled(detail, Style::default().fg(theme.muted)),
            ]));
            if (expanded || force_expand) && !activity_tail.is_empty() {
                for item in activity_tail {
                    lines.push(Line::from(vec![
                        Span::styled("  │   ", Style::default().fg(theme.border)),
                        Span::styled(item.clone(), Style::default().fg(theme.muted)),
                    ]));
                }
            }
        }
        (_, _, Some(err)) => {
            lines.push(Line::from(vec![
                Span::styled("  │ error: ", Style::default().fg(theme.error)),
                Span::styled(
                    truncate_lines(err, body_limit),
                    Style::default().fg(theme.error),
                ),
            ]));
        }
        (_, Some(out), _) => {
            lines.push(Line::from(vec![
                Span::styled("  │ result: ", Style::default().fg(theme.success)),
                Span::styled(
                    truncate_lines(out, body_limit),
                    Style::default().fg(theme.muted),
                ),
            ]));
        }
        _ => {}
    }

    lines.push(Line::from(Span::styled(
        "  └─ returned to main agent",
        Style::default().fg(theme.muted),
    )));
}

fn append_diff_lines(
    lines: &mut Vec<Line<'static>>,
    diff: &str,
    theme: &Theme,
    refined: Option<&crate::diff_highlight::RefinedDiffStyles>,
) {
    lines.extend(crate::diff_highlight::render_unified_diff(
        diff, theme, refined,
    ));
}

fn append_tool_output_lines(
    lines: &mut Vec<Line<'static>>,
    output: &str,
    expanded: bool,
    force_expand: bool,
    theme: &Theme,
) {
    // Flushing to the immutable scrollback (`force_expand`) commits the entire output so
    // nothing is hidden behind a "[+N lines]" stub that `e` can no longer reveal. The live
    // pane caps the `e`-expanded view at 40 rows and the collapsed view at 2.
    let max_lines = if force_expand {
        usize::MAX
    } else if expanded {
        40
    } else {
        2
    };
    let mut output_lines = output.lines();
    for line in output_lines.by_ref().take(max_lines) {
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(theme.muted),
        )));
    }

    let hidden = output_lines.count();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("    [+{hidden} lines]"),
            Style::default().fg(theme.muted),
        )));
    }
}

fn spinner_frame(tick: u64) -> &'static str {
    const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    SPINNER_FRAMES[((tick / 2) as usize) % SPINNER_FRAMES.len()]
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval required",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_status_color(status: TaskStatus, theme: &Theme) -> Color {
    match status {
        TaskStatus::Running | TaskStatus::Stopping => theme.warning,
        TaskStatus::Completed => theme.success,
        TaskStatus::Failed | TaskStatus::Cancelled => theme.error,
        TaskStatus::ApprovalRequired => theme.warning,
        TaskStatus::Queued | TaskStatus::Paused | TaskStatus::Stopped => theme.muted,
    }
}

fn render_input(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    layout: &TextareaVisualLayout,
    state: &AppState,
    theme: &Theme,
    show_hardware_cursor: bool,
) {
    render_textarea_surface(
        frame,
        area,
        textarea,
        Some(layout),
        state.copy_notice_at(std::time::Instant::now()),
        theme,
        show_hardware_cursor,
    );
}

fn render_search_bar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.is_empty() {
        return;
    }
    let count = format!(
        " {}/{} ",
        state.transcript_search.active_ordinal().unwrap_or(0),
        state.transcript_search.match_count()
    );
    let count_width = UnicodeWidthStr::width(count.as_str()).min(area.width as usize) as u16;
    let prefix = if area.width.saturating_sub(count_width) >= 7 {
        " Find: "
    } else {
        "F:"
    };
    let prefix_width = UnicodeWidthStr::width(prefix).min(area.width as usize) as u16;
    let query_width = area.width.saturating_sub(prefix_width + count_width);

    if prefix_width > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(prefix, Style::default().fg(theme.muted))),
            Rect::new(area.x, area.y, prefix_width, 1),
        );
    }
    if count_width > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(count, Style::default().fg(theme.muted))),
            Rect::new(area.right() - count_width, area.y, count_width, 1),
        );
    }
    if query_width == 0 {
        return;
    }

    let mut textarea = TextArea::from([state.transcript_search.query()]);
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(
        Style::default()
            .fg(theme.border)
            .add_modifier(Modifier::REVERSED),
    );
    let cursor_column = state.transcript_search.query()[..state.transcript_search.cursor()]
        .chars()
        .count()
        .min(u16::MAX as usize) as u16;
    textarea.move_cursor(tui_textarea::CursorMove::Jump(0, cursor_column));
    render_textarea_surface(
        frame,
        Rect::new(area.x + prefix_width, area.y, query_width, 1),
        &textarea,
        None,
        None,
        theme,
        !state.show_shortcuts
            && state.status != AppStatus::WaitingApproval
            && state.plan_approval_dialog.is_none(),
    );
}

fn render_textarea_surface(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    precomputed_layout: Option<&TextareaVisualLayout>,
    notice: Option<CopyNotice>,
    theme: &Theme,
    show_hardware_cursor: bool,
) {
    let inner = render_textarea_block_and_notice(frame, area, textarea, notice, theme);
    if inner.is_empty() {
        return;
    }

    let computed_layout;
    let layout = if let Some(layout) = precomputed_layout {
        layout
    } else {
        computed_layout = textarea_visual_layout_with_selection(
            textarea,
            inner.width as usize,
            theme.selection_style(),
        );
        &computed_layout
    };
    let visible_height = inner.height as usize;
    let start = textarea_visible_start(layout, visible_height);
    let end = (start + visible_height).min(layout.lines.len());
    let visible = layout.lines[start..end].to_vec();
    let paragraph = Paragraph::new(visible)
        .style(textarea.style())
        .alignment(layout.alignment);
    frame.render_widget(paragraph, inner);

    if show_hardware_cursor && let Some(position) = visible_textarea_cursor(layout, inner) {
        frame.set_cursor_position(position);
    }
}

fn render_textarea_block_and_notice(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    notice: Option<CopyNotice>,
    theme: &Theme,
) -> Rect {
    let inner = if let Some(block) = textarea.block() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };

    // Transient "copied N chars" feedback overlays the right end of the top
    // border, mirroring the " Input " title on the left.
    if let Some(notice) = notice {
        let text = if notice.local_only {
            format!(" copied {} chars (local clipboard only) ", notice.chars)
        } else {
            format!(" copied {} chars to clipboard ", notice.chars)
        };
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        // Keep one border cell visible on each side of the overlay.
        if area.height > 0 && text_width + 2 < area.width {
            let overlay = Rect::new(area.x + area.width - text_width - 2, area.y, text_width, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(text, Style::default().fg(theme.approval))),
                overlay,
            );
        }
    }

    inner
}

fn composer_input_height(
    area_width: u16,
    textarea: &TextArea,
    layout: &TextareaVisualLayout,
) -> u16 {
    let input_lines = layout.lines.len().max(1) as u16;
    let block_extra = textarea
        .block()
        .map(|block| {
            let outer = Rect::new(0, 0, area_width, u16::MAX);
            u16::MAX.saturating_sub(block.inner(outer).height)
        })
        .unwrap_or(0);
    input_lines.saturating_add(block_extra)
}

fn composer_visual_layout(
    area_width: u16,
    textarea: &TextArea,
    theme: &Theme,
) -> TextareaVisualLayout {
    let inner_width = textarea_inner_width(area_width, textarea) as usize;
    textarea_visual_layout_with_selection(textarea, inner_width, theme.selection_style())
}

fn textarea_inner_width(area_width: u16, textarea: &TextArea) -> u16 {
    textarea
        .block()
        .map(|block| block.inner(Rect::new(0, 0, area_width, 1)).width)
        .unwrap_or(area_width)
}

struct TextareaVisualLayout {
    lines: Vec<Line<'static>>,
    cursor_visual_row: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    cursor_display_col: usize,
    cursor_cell_width: usize,
    alignment: Alignment,
    rows: Vec<TextareaVisualRow>,
}

struct TextareaVisualRow {
    logical_row: usize,
    logical_range: Range<usize>,
    graphemes: Vec<TextareaHitGrapheme>,
}

struct TextareaHitGrapheme {
    logical_range: Range<usize>,
    width: usize,
}

#[derive(Clone, Copy)]
enum TextareaCursorCell {
    Occupied { display_col: usize, width: usize },
    Space,
}

impl TextareaCursorCell {
    fn display_col(self, raw_display_col: usize) -> usize {
        match self {
            Self::Occupied { display_col, .. } => display_col,
            Self::Space => raw_display_col,
        }
    }

    fn width(self) -> usize {
        match self {
            Self::Occupied { width, .. } => width,
            Self::Space => 1,
        }
    }
}

fn textarea_visual_layout(textarea: &TextArea, width: usize) -> TextareaVisualLayout {
    textarea_visual_layout_with_selection(textarea, width, Style::default().bg(Color::LightBlue))
}

fn textarea_visual_layout_with_selection(
    textarea: &TextArea,
    width: usize,
    selection_style: Style,
) -> TextareaVisualLayout {
    if textarea.is_empty() {
        let mut spans = vec![Span::styled(" ", textarea.cursor_style())];
        if let Some(style) = textarea.placeholder_style() {
            spans.push(Span::styled(textarea.placeholder_text().to_string(), style));
        }
        return TextareaVisualLayout {
            lines: vec![Line::from(spans)],
            cursor_visual_row: 0,
            cursor_display_col: 0,
            cursor_cell_width: 1,
            alignment: textarea.alignment(),
            rows: vec![TextareaVisualRow {
                logical_row: 0,
                logical_range: 0..0,
                graphemes: Vec::new(),
            }],
        };
    }

    let (cursor_row, cursor_col) = textarea.cursor();
    let selection = textarea.selection_range();
    let mut visual_lines = Vec::new();
    let mut visual_rows = Vec::new();
    let mut cursor_visual_line = 0usize;
    let mut cursor_display_col = 0usize;
    let mut cursor_cell_width = 0usize;

    for (row, original_line) in textarea.lines().iter().enumerate() {
        let display_line = textarea_display_line(textarea, original_line);
        let graphemes = textarea_graphemes(&display_line);
        let ranges = textarea_wrap_ranges(&graphemes, display_line.chars().count(), width);
        for (range_index, range) in ranges.iter().enumerate() {
            let range_graphemes = textarea_graphemes_in_range(&graphemes, range);
            let visual_index = visual_lines.len();
            let is_last_range = range_index + 1 == ranges.len();
            let contains_cursor = row == cursor_row
                && cursor_in_visual_range(
                    cursor_col,
                    range,
                    is_last_range,
                    original_line.chars().count(),
                );
            let raw_cursor_display_col = contains_cursor
                .then(|| textarea_display_width(&display_line, range.start, cursor_col));
            let cursor_inside_grapheme = contains_cursor
                && range_graphemes.iter().any(|grapheme| {
                    grapheme.logical_range.start < cursor_col
                        && cursor_col < grapheme.logical_range.end
                });
            let needs_synthetic_cursor = raw_cursor_display_col
                .is_some_and(|display_col| width > 0 && display_col >= width)
                && !cursor_inside_grapheme;
            let cursor_cell = raw_cursor_display_col.and_then(|raw_display_col| {
                if needs_synthetic_cursor || width == 0 {
                    None
                } else {
                    let cursor_cell =
                        textarea_cursor_cell(range_graphemes, cursor_col, raw_display_col);
                    cursor_visual_line = visual_index;
                    cursor_display_col = cursor_cell.display_col(raw_display_col);
                    cursor_cell_width = cursor_cell.width();
                    Some(cursor_cell)
                }
            });
            visual_lines.push(render_textarea_visual_line(
                original_line,
                row,
                range.clone(),
                textarea,
                selection,
                cursor_cell,
                range_graphemes,
                selection_style,
            ));
            visual_rows.push(TextareaVisualRow {
                logical_row: row,
                logical_range: range.clone(),
                graphemes: range_graphemes
                    .iter()
                    .map(|grapheme| TextareaHitGrapheme {
                        logical_range: grapheme.logical_range.clone(),
                        width: grapheme.width,
                    })
                    .collect(),
            });
            if needs_synthetic_cursor {
                cursor_visual_line = visual_lines.len();
                cursor_display_col = 0;
                cursor_cell_width = 1;
                visual_lines.push(Line::from(Span::styled(" ", textarea.cursor_style())));
                visual_rows.push(TextareaVisualRow {
                    logical_row: row,
                    logical_range: cursor_col..cursor_col,
                    graphemes: Vec::new(),
                });
            }
        }
    }

    if visual_lines.is_empty() {
        visual_lines.push(Line::from(Span::styled(" ", textarea.cursor_style())));
        visual_rows.push(TextareaVisualRow {
            logical_row: 0,
            logical_range: 0..0,
            graphemes: Vec::new(),
        });
    }

    TextareaVisualLayout {
        lines: visual_lines,
        cursor_visual_row: cursor_visual_line,
        cursor_display_col,
        cursor_cell_width,
        alignment: textarea.alignment(),
        rows: visual_rows,
    }
}

fn textarea_display_line(textarea: &TextArea, logical_line: &str) -> String {
    match textarea.mask_char() {
        Some(mask) => std::iter::repeat(mask)
            .take(logical_line.chars().count())
            .collect(),
        None => logical_line.to_string(),
    }
}

struct TextareaGrapheme<'a> {
    text: &'a str,
    logical_range: Range<usize>,
    width: usize,
}

#[cfg(test)]
thread_local! {
    static TEXTAREA_GRAPHEME_TOKENIZATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_textarea_grapheme_tokenization_count() {
    TEXTAREA_GRAPHEME_TOKENIZATIONS.set(0);
}

#[cfg(test)]
fn textarea_grapheme_tokenization_count() -> usize {
    TEXTAREA_GRAPHEME_TOKENIZATIONS.get()
}

fn textarea_graphemes(line: &str) -> Vec<TextareaGrapheme<'_>> {
    #[cfg(test)]
    TEXTAREA_GRAPHEME_TOKENIZATIONS.with(|count| count.set(count.get() + 1));
    let mut logical_start = 0usize;
    line.graphemes(true)
        .map(|text| {
            let logical_end = logical_start + text.chars().count();
            let grapheme = TextareaGrapheme {
                text,
                logical_range: logical_start..logical_end,
                width: UnicodeWidthStr::width(text),
            };
            logical_start = logical_end;
            grapheme
        })
        .collect()
}

fn textarea_graphemes_in_range<'a>(
    graphemes: &'a [TextareaGrapheme<'a>],
    range: &Range<usize>,
) -> &'a [TextareaGrapheme<'a>] {
    let start = graphemes.partition_point(|grapheme| grapheme.logical_range.end <= range.start);
    let end = graphemes.partition_point(|grapheme| grapheme.logical_range.start < range.end);
    &graphemes[start.min(end)..end]
}

fn cursor_in_visual_range(
    cursor_col: usize,
    range: &Range<usize>,
    is_last_range: bool,
    line_len: usize,
) -> bool {
    range.contains(&cursor_col) || (is_last_range && cursor_col == line_len)
}

fn textarea_display_width(display_line: &str, start: usize, end: usize) -> usize {
    UnicodeWidthStr::width(textarea_char_slice(display_line, start, end))
}

fn textarea_char_slice(text: &str, start: usize, end: usize) -> &str {
    let start = textarea_char_byte_index(text, start);
    let end = textarea_char_byte_index(text, end);
    &text[start.min(end)..end]
}

fn textarea_char_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte_index, _)| byte_index)
}

fn textarea_cursor_cell(
    graphemes: &[TextareaGrapheme<'_>],
    cursor_col: usize,
    raw_display_col: usize,
) -> TextareaCursorCell {
    let mut display_col = 0usize;
    let mut cursor_in_zero_width_grapheme = false;
    for grapheme in graphemes {
        let next_display_col = display_col + grapheme.width;
        if grapheme.width == 0 {
            cursor_in_zero_width_grapheme |= grapheme.logical_range.contains(&cursor_col);
        } else if grapheme.logical_range.contains(&cursor_col)
            || cursor_in_zero_width_grapheme
            || raw_display_col < next_display_col
        {
            return TextareaCursorCell::Occupied {
                display_col,
                width: grapheme.width,
            };
        }
        display_col = next_display_col;
    }
    TextareaCursorCell::Space
}

fn textarea_visible_start(layout: &TextareaVisualLayout, visible_height: usize) -> usize {
    if layout.lines.len() <= visible_height {
        0
    } else if layout.cursor_visual_row >= visible_height {
        layout.cursor_visual_row + 1 - visible_height
    } else {
        0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn visible_textarea_cursor(layout: &TextareaVisualLayout, inner: Rect) -> Option<Position> {
    if inner.is_empty()
        || layout.alignment != Alignment::Left
        || layout.cursor_visual_row >= layout.lines.len()
        || layout
            .cursor_display_col
            .checked_add(layout.cursor_cell_width)?
            > inner.width as usize
    {
        return None;
    }
    let start = textarea_visible_start(layout, inner.height as usize);
    let row = layout.cursor_visual_row.checked_sub(start)?;
    if row >= inner.height as usize {
        return None;
    }
    Some(Position::new(
        inner
            .x
            .checked_add(layout.cursor_display_col.try_into().ok()?)?,
        inner.y.checked_add(row.try_into().ok()?)?,
    ))
}

fn render_textarea_visual_line(
    original_line: &str,
    row: usize,
    range: Range<usize>,
    textarea: &TextArea,
    selection: Option<((usize, usize), (usize, usize))>,
    cursor_cell: Option<TextareaCursorCell>,
    graphemes: &[TextareaGrapheme<'_>],
    selection_style: Style,
) -> Line<'static> {
    let base_style = textarea.style();
    let cursor_style = textarea.cursor_style();
    let cursor_line_style = textarea.cursor_line_style();
    let mut spans = Vec::new();
    let mut pending = String::new();
    let mut pending_style = base_style;
    let mut attached_zero_width = String::new();
    let mut attached_logical_start = None;
    let mut rendered_visible_grapheme = false;
    let mut display_col = 0usize;

    for grapheme in graphemes {
        if grapheme.width == 0 {
            attached_logical_start.get_or_insert(grapheme.logical_range.start);
            attached_zero_width.push_str(grapheme.text);
            continue;
        }
        let logical_range = attached_logical_start
            .take()
            .unwrap_or(grapheme.logical_range.start)
            ..grapheme.logical_range.end;
        let next_display_col = display_col + grapheme.width;
        let style = if matches!(
            cursor_cell,
            Some(TextareaCursorCell::Occupied {
                display_col: cursor_col,
                ..
            }) if display_col == cursor_col
        ) {
            cursor_style
        } else if logical_range
            .clone()
            .any(|col| selection_contains(selection, row, col))
        {
            selection_style
        } else if row == textarea.cursor().0 {
            cursor_line_style
        } else {
            base_style
        };
        let mut rendered_grapheme = std::mem::take(&mut attached_zero_width);
        rendered_grapheme.push_str(grapheme.text);
        attached_zero_width.clear();
        push_styled_text(
            &mut spans,
            &mut pending,
            &mut pending_style,
            &rendered_grapheme,
            style,
        );
        rendered_visible_grapheme = true;
        display_col = next_display_col;
    }

    let zero_width_cursor_cell = !attached_zero_width.is_empty() && !rendered_visible_grapheme;
    if zero_width_cursor_cell {
        let logical_start = attached_logical_start.unwrap_or(range.end);
        let logical_range = logical_start..range.end;
        let style = if cursor_cell.is_some() {
            cursor_style
        } else if logical_range
            .clone()
            .any(|col| selection_contains(selection, row, col))
        {
            selection_style
        } else if row == textarea.cursor().0 {
            cursor_line_style
        } else {
            base_style
        };
        let mut cursor_cell = String::from(" ");
        cursor_cell.push_str(&attached_zero_width);
        push_styled_text(
            &mut spans,
            &mut pending,
            &mut pending_style,
            &cursor_cell,
            style,
        );
    } else if !attached_zero_width.is_empty() {
        pending.push_str(&attached_zero_width);
    }
    flush_pending_span(&mut spans, &mut pending, pending_style);

    if matches!(cursor_cell, Some(TextareaCursorCell::Space)) && !zero_width_cursor_cell {
        spans.push(Span::styled(" ", cursor_style));
    } else if selection_contains(selection, row, range.end)
        && range.end == original_line.chars().count()
    {
        spans.push(Span::styled(" ", selection_style));
    }

    Line::from(spans)
}

fn selection_contains(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    col: usize,
) -> bool {
    let Some(((start_row, start_col), (end_row, end_col))) = selection else {
        return false;
    };
    (row > start_row || (row == start_row && col >= start_col))
        && (row < end_row || (row == end_row && col < end_col))
}

fn push_styled_text(
    spans: &mut Vec<Span<'static>>,
    pending: &mut String,
    pending_style: &mut Style,
    text: &str,
    style: Style,
) {
    if pending.is_empty() {
        *pending_style = style;
    } else if *pending_style != style {
        flush_pending_span(spans, pending, *pending_style);
        *pending_style = style;
    }
    pending.push_str(text);
}

fn flush_pending_span(spans: &mut Vec<Span<'static>>, pending: &mut String, pending_style: Style) {
    if !pending.is_empty() {
        spans.push(Span::styled(std::mem::take(pending), pending_style));
    }
}

fn textarea_wrap_ranges(
    graphemes: &[TextareaGrapheme<'_>],
    line_len: usize,
    width: usize,
) -> Vec<Range<usize>> {
    if graphemes.is_empty() || width == 0 {
        return vec![0..line_len];
    }

    let mut ranges = Vec::new();
    let mut current_start = None;
    let mut current_end = 0usize;
    let mut current_width = 0usize;
    let mut segment_start = 0;

    for segment_end in 1..=graphemes.len() {
        let final_grapheme = &graphemes[segment_end - 1];
        if segment_end < graphemes.len() && !textarea_grapheme_ends_segment(final_grapheme) {
            continue;
        }
        let segment = &graphemes[segment_start..segment_end];
        let segment_logical_start = segment[0].logical_range.start;
        let segment_logical_end = segment
            .last()
            .map(|grapheme| grapheme.logical_range.end)
            .unwrap_or(segment_logical_start);
        let segment_width = segment.iter().map(|grapheme| grapheme.width).sum::<usize>();

        if segment_width > width {
            if let Some(start) = current_start.take() {
                ranges.push(start..current_end);
            }
            let (start, end, display_width) =
                push_hard_wrapped_segment(&mut ranges, segment, width);
            current_start = Some(start);
            current_end = end;
            current_width = display_width;
        } else if current_start.is_none() {
            current_start = Some(segment_logical_start);
            current_end = segment_logical_end;
            current_width = segment_width;
        } else if current_width + segment_width <= width {
            current_end = segment_logical_end;
            current_width += segment_width;
        } else {
            ranges.push(current_start.unwrap_or(segment_logical_start)..current_end);
            current_start = Some(segment_logical_start);
            current_end = segment_logical_end;
            current_width = segment_width;
        }

        segment_start = segment_end;
    }

    if let Some(start) = current_start {
        ranges.push(start..current_end);
    } else if ranges.is_empty() {
        ranges.push(0..line_len);
    }
    ranges
}

fn textarea_grapheme_ends_segment(grapheme: &TextareaGrapheme<'_>) -> bool {
    grapheme
        .text
        .chars()
        .next()
        .is_some_and(|ch| ch.is_whitespace() || ch == '/' || ch == '-')
}

fn push_hard_wrapped_segment(
    ranges: &mut Vec<Range<usize>>,
    segment: &[TextareaGrapheme<'_>],
    width: usize,
) -> (usize, usize, usize) {
    let mut chunk_start = segment
        .first()
        .map(|grapheme| grapheme.logical_range.start)
        .unwrap_or(0);
    let mut current_col = chunk_start;
    let mut current_width = 0usize;

    for grapheme in segment {
        if current_width > 0 && grapheme.width > 0 && current_width + grapheme.width > width {
            ranges.push(chunk_start..current_col);
            chunk_start = grapheme.logical_range.start;
            current_width = 0;
        }
        current_col = grapheme.logical_range.end;
        current_width += grapheme.width;
    }

    (chunk_start, current_col, current_width)
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // The live status dot + elapsed time moved to the activity line above the composer
    // (see `render_activity`); this bottom line is now purely persistent metadata.
    let line = status_line(state, theme, area.width as usize);

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

fn workspace_status_spans(
    state: &AppState,
    theme: &Theme,
    available_width: usize,
) -> Vec<Span<'static>> {
    let separator = "  ·  ";
    let separator_width = UnicodeWidthStr::width(separator);
    if available_width <= separator_width {
        return Vec::new();
    }

    let git_label = state.workspace_git.as_ref().map(GitIdentity::label);
    let git_width = git_label
        .as_deref()
        .map(|label| separator_width + UnicodeWidthStr::width(label))
        .unwrap_or(0);

    let make_spans = |cwd: String, git: Option<String>| {
        let mut spans = vec![Span::styled(
            format!("{separator}{cwd}"),
            Style::default().fg(theme.muted),
        )];
        if let Some(git) = git {
            spans.push(Span::styled(
                format!("{separator}{git}"),
                Style::default().fg(theme.muted),
            ));
        }
        spans
    };

    if let Some(git) = git_label.as_ref()
        && available_width > separator_width + git_width
    {
        let cwd = compact_cwd(
            &state.cwd,
            available_width
                .saturating_sub(separator_width)
                .saturating_sub(git_width),
        );
        if !cwd.is_empty() {
            return make_spans(cwd, Some(git.clone()));
        }
    }

    let cwd = compact_cwd(&state.cwd, available_width - separator_width);
    if cwd.is_empty() {
        Vec::new()
    } else {
        make_spans(cwd, None)
    }
}

fn status_line(state: &AppState, theme: &Theme, width: usize) -> Line<'static> {
    if state.side_conversation_active()
        && let Some(side) = state.side_conversation.as_ref()
    {
        let label = format!(
            " Side from main · {} · Ctrl+/ to switch · Ctrl+C to close",
            side.parent_status.label()
        );
        return Line::from(Span::styled(
            truncate_to_display_width(&label, width),
            Style::default().fg(theme.plan_mode),
        ));
    }
    if state.side_conversation_available() {
        let label = " Main · Side available · Ctrl+/ to switch";
        return Line::from(Span::styled(
            truncate_to_display_width(label, width),
            Style::default().fg(theme.plan_mode),
        ));
    }
    let separator = "  ·  ";
    let mode_prefix = separator;
    let mode_value = state.approval_mode.as_str();
    let mode_width = UnicodeWidthStr::width(mode_prefix) + UnicodeWidthStr::width(mode_value);
    let context = (state.context_limit_tokens > 0).then(|| context_cell(state, theme));
    let reserved_context_width = context
        .as_ref()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .filter(|context_width| mode_width + context_width <= width)
        .unwrap_or(0);
    let model = format!(
        " {} ({})",
        state.model_name,
        state.reasoning_effort.as_str()
    );
    let model = truncate_to_display_width(
        &model,
        width
            .saturating_sub(mode_width)
            .saturating_sub(reserved_context_width),
    );
    let mut used = UnicodeWidthStr::width(model.as_str()) + mode_width;
    let mut spans = vec![
        Span::styled(model, Style::default().fg(theme.muted)),
        Span::styled(mode_prefix, Style::default().fg(theme.muted)),
        Span::styled(
            mode_value,
            Style::default().fg(approval_mode_color(state.approval_mode, theme)),
        ),
    ];

    if let Some(context) = context {
        let context_width = UnicodeWidthStr::width(context.content.as_ref());
        if used + context_width <= width {
            used += context_width;
            spans.push(context);
        }
    }

    for span in workspace_status_spans(state, theme, width.saturating_sub(used)) {
        used += UnicodeWidthStr::width(span.content.as_ref());
        spans.push(span);
    }

    let mut lower_priority = Vec::new();
    // Session cost only appears once there is something to report; a fresh
    // session keeps the bar clean instead of showing zeros.
    if state.usage.total_tokens() > 0 {
        lower_priority.push(Span::styled(
            format!(
                "{separator}{} tokens{separator}{}",
                format_token_count(state.usage.total_tokens()),
                format_cost(state.usage.estimated_cost_usd),
            ),
            Style::default().fg(theme.muted),
        ));
    }
    lower_priority.push(Span::styled(
        format!("{separator}F1 shortcuts"),
        Style::default().fg(theme.muted),
    ));

    for span in lower_priority {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= width {
            used += span_width;
            spans.push(span);
        }
    }

    Line::from(spans)
}

/// Humanize token counts for the status bar: 950 → "950", 8_664 → "8.7k",
/// 1_250_000 → "1.3M". Full precision lives in `/cost`.
fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

/// Session cost with just enough precision to be meaningful: sub-cent costs
/// show four decimals, anything larger the familiar two.
fn format_cost(cost_usd: f64) -> String {
    if cost_usd < 0.01 {
        format!("${cost_usd:.4}")
    } else {
        format!("${cost_usd:.2}")
    }
}

fn approval_mode_color(mode: ApprovalMode, theme: &Theme) -> Color {
    match mode {
        ApprovalMode::Suggest => theme.border,
        ApprovalMode::AutoEdit => theme.approval,
        ApprovalMode::FullAuto => theme.error,
        ApprovalMode::Plan => theme.plan_mode,
    }
}

/// The activity indicator shown on its own line directly above the composer. Returns
/// `None` while idle so the line collapses to zero height and a resting session stays
/// clean; every other status renders a coloured dot, a label, and (while running) the
/// elapsed wall-clock time.
fn activity_line(state: &AppState, theme: &Theme) -> Option<(String, ratatui::style::Color)> {
    match &state.status {
        AppStatus::Idle => background_task_activity_line(&state.workflow_panel.tasks, theme),
        AppStatus::Setup | AppStatus::SessionPicker => None,
        AppStatus::Running => {
            let live_elapsed = state
                .running_started_at
                .map(|started| started.elapsed().as_secs())
                .unwrap_or_default();
            let persisted_goal_elapsed = state
                .current_goal
                .as_ref()
                .filter(|goal| goal.status.should_continue())
                .map(|goal| goal.time_used_seconds.max(0) as u64)
                .unwrap_or_default();
            let elapsed =
                format_elapsed_compact(persisted_goal_elapsed.saturating_add(live_elapsed));
            Some((format!("● running {elapsed}"), theme.warning))
        }
        AppStatus::Compacting => Some(("● Compacting context...".to_string(), theme.warning)),
        AppStatus::WaitingApproval => Some(("● approval".to_string(), theme.approval)),
        AppStatus::WaitingUserInput => Some(("● input".to_string(), theme.approval)),
    }
}

fn background_task_activity_line(
    tasks: &[BackgroundTaskSummary],
    theme: &Theme,
) -> Option<(String, ratatui::style::Color)> {
    let activity = TaskActivitySummary::from_tasks(tasks);
    if !activity.has_active_tasks() && !activity.requires_attention() {
        return None;
    }

    let mut labels = Vec::with_capacity(2);
    if activity.active_count > 0 {
        let noun = if activity.active_count == 1 {
            "task"
        } else {
            "tasks"
        };
        labels.push(format!(
            "{} background {noun} running",
            activity.active_count
        ));
    }
    if activity.attention_count > 0 {
        let verb = if activity.attention_count == 1 {
            "needs"
        } else {
            "need"
        };
        labels.push(format!("{} {verb} approval", activity.attention_count));
    }

    let color = if activity.requires_attention() {
        theme.approval
    } else {
        theme.warning
    };
    Some((format!("● {}", labels.join(" · ")), color))
}

fn render_activity(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some((text, color)) = activity_line(state, theme) else {
        return;
    };
    // First row stays blank as a spacer between the transcript tail and the indicator.
    let paragraph = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(format!(" {text}"), Style::default().fg(color))),
    ]);
    frame.render_widget(paragraph, area);
}

fn format_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

/// Remaining context as a percentage of the full model window (100% = empty).
/// Fed by the provider-reported prompt tokens once a turn completes; a fresh
/// session reads high. Pure local observability — never sent upstream, so it
/// cannot affect DeepSeek's prefix cache. Hidden until a real budget is known.
fn context_cell(state: &AppState, theme: &Theme) -> Span<'static> {
    if state.context_limit_tokens == 0 {
        return Span::raw("");
    }
    let used = state.context_used_tokens.min(state.context_limit_tokens);
    let remaining = state.context_limit_tokens.saturating_sub(used);
    let percent = (remaining * 100) / state.context_limit_tokens;
    let color = if percent > 50 {
        theme.success
    } else if percent > 20 {
        theme.warning
    } else {
        theme.error
    };
    Span::styled(
        format!("  ·  context {percent}%"),
        Style::default().fg(color),
    )
}

fn render_shortcuts(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = frame.area();
    let width = 58u16.min(area.width.saturating_sub(4));
    let max_height = area.height.saturating_sub(4);
    let scopes = active_shortcut_scopes(state);
    let lines = shortcuts::shortcut_lines(&scopes);
    let height = ((lines.len() as u16) + 2).min(max_height).max(3);
    let popup_area = centered_rect(area, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Shortcuts ")
        .border_style(Style::default().fg(theme.border));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup_area);
}

fn active_shortcut_scopes(state: &AppState) -> Vec<ShortcutScope> {
    match state.status {
        AppStatus::Idle => vec![ShortcutScope::Global, ShortcutScope::Idle],
        AppStatus::Running | AppStatus::Compacting => {
            vec![ShortcutScope::Global, ShortcutScope::Running]
        }
        AppStatus::WaitingApproval => vec![ShortcutScope::Global, ShortcutScope::Approval],
        AppStatus::WaitingUserInput => vec![ShortcutScope::Global, ShortcutScope::Idle],
        AppStatus::Setup | AppStatus::SessionPicker => vec![ShortcutScope::Global],
    }
}

/// The scrolled item window both popup menus (slash, mention) use: the
/// selected row stays visible, pinned to the window's bottom while moving
/// down.
fn popup_window(len: usize, selected: usize, max_visible: usize) -> (usize, usize) {
    let visible_count = len.min(12).min(max_visible);
    let max_start = len.saturating_sub(visible_count);
    let start = selected
        .saturating_sub(visible_count.saturating_sub(1))
        .min(max_start);
    (start, (start + visible_count).min(len))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PopupGeometry {
    area: Rect,
    start: usize,
    end: usize,
    show_status: bool,
}

fn popup_geometry(
    frame_area: Rect,
    input_area: Rect,
    len: usize,
    selected: usize,
    wants_status: bool,
) -> Option<PopupGeometry> {
    let left = input_area.x.max(frame_area.x);
    let right = input_area.right().min(frame_area.right());
    let width = right.checked_sub(left)?;
    let bottom = input_area.y.clamp(frame_area.y, frame_area.bottom());
    let available_height = bottom.saturating_sub(frame_area.y);
    if width == 0 || available_height < 2 {
        return None;
    }

    let max_content_rows = available_height.saturating_sub(2) as usize;
    let show_status = wants_status && max_content_rows > 0 && (len == 0 || max_content_rows >= 2);
    let max_items = max_content_rows.saturating_sub(usize::from(show_status));
    let (start, end) = popup_window(len, selected, max_items);
    let height = (end - start)
        .saturating_add(usize::from(show_status))
        .saturating_add(2)
        .min(available_height as usize) as u16;

    Some(PopupGeometry {
        area: Rect::new(left, bottom - height, width, height),
        start,
        end,
        show_status,
    })
}

/// Which slash-menu item (of the active list — sub-menu when open) a click
/// lands on.
pub(crate) fn slash_menu_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    let menu = state.slash_menu.as_ref()?;
    let frame_area = state.frame_area?;
    let input_area = state.input_area?;
    let (len, selected) = match &menu.sub_menu {
        Some(sub) => (sub.items.len(), sub.selected),
        None => (menu.items.len(), menu.selected),
    };
    let geometry = popup_geometry(frame_area, input_area, len, selected, false)?;
    hit_bordered_list_row(geometry.area, column, row).and_then(|offset| {
        let index = geometry.start + offset;
        (index < geometry.end).then_some(index)
    })
}

/// Which mention candidate a click lands on, replicating
/// `render_mention_candidates` geometry. The trailing status row (if any)
/// is not a candidate and reports `None`.
pub(crate) fn mention_menu_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    // The mention popup only renders while no slash menu is open; the
    // hit-test must honor the same gate.
    if state.slash_menu.is_some() {
        return None;
    }
    state.mention.phase.as_ref()?;
    let frame_area = state.frame_area?;
    let input_area = state.input_area?;
    let candidates = &state.mention.candidates;
    let status = mention_popup_status(state);
    let geometry = popup_geometry(
        frame_area,
        input_area,
        candidates.len(),
        state.mention.selected,
        status.is_some(),
    )?;
    hit_bordered_list_row(geometry.area, column, row).and_then(|offset| {
        let index = geometry.start + offset;
        (index < geometry.end).then_some(index)
    })
}

/// Row offset within a bordered single-column list popup, if `column`/`row`
/// land inside its content area.
fn hit_bordered_list_row(popup: Rect, column: u16, row: u16) -> Option<usize> {
    if popup.width < 3 || popup.height < 3 {
        return None;
    }
    let content_left = popup.x + 1;
    let content_right = popup.x + popup.width - 1;
    let content_top = popup.y + 1;
    let content_bottom = popup.y + popup.height - 1;
    (column >= content_left && column < content_right && row >= content_top && row < content_bottom)
        .then(|| (row - content_top) as usize)
}

fn render_slash_menu(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let menu = match &state.slash_menu {
        Some(m) => m,
        None => return,
    };

    // Determine items and title based on sub-menu state
    let (items, selected, title): (Vec<(&str, &str)>, usize, &str) =
        if let Some(sub) = &menu.sub_menu {
            let items: Vec<(&str, &str)> = sub.items.iter().map(|s| (s.as_str(), "")).collect();
            (items, sub.selected, &sub.title)
        } else {
            let items: Vec<(&str, &str)> = menu
                .items
                .iter()
                .map(|i| (i.command.as_str(), i.description.as_str()))
                .collect();
            (items, menu.selected, " Commands ")
        };

    let Some(geometry) = popup_geometry(frame.area(), input_area, items.len(), selected, false)
    else {
        return;
    };

    frame.render_widget(Clear, geometry.area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (cmd, desc)) in items[geometry.start..geometry.end].iter().enumerate() {
        let item_index = geometry.start + i;
        let prefix = if item_index == selected { "▸ " } else { "  " };
        let style = if item_index == selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        if desc.is_empty() {
            lines.push(Line::from(Span::styled(format!("{prefix}{cmd}"), style)));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}{cmd}"), style),
                Span::styled(format!("  {desc}"), Style::default().fg(theme.muted)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(theme.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, geometry.area);
}

fn render_mention_candidates(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let candidates = &state.mention.candidates;
    if state.mention.phase.is_none() {
        return;
    }

    let skills_only = state.mention.sigil == Some(orca_runtime::mentions::MentionSigil::Dollar);
    let status = mention_popup_status(state);
    let Some(geometry) = popup_geometry(
        frame.area(),
        input_area,
        candidates.len(),
        state.mention.selected,
        status.is_some(),
    ) else {
        return;
    };

    frame.render_widget(Clear, geometry.area);

    let mut lines: Vec<Line> = candidates
        .iter()
        .enumerate()
        .skip(geometry.start)
        .take(geometry.end.saturating_sub(geometry.start))
        .map(|(i, candidate)| {
            let prefix = if i == state.mention.selected {
                "▸ "
            } else {
                "  "
            };
            let style = if i == state.mention.selected {
                Style::default()
                    .fg(theme.border)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let sigil = state.mention.sigil.map_or('@', |sigil| sigil.as_char());
            let mut spans = vec![Span::styled(format!("{prefix}{sigil}"), style)];
            for (index, ch) in candidate.display.chars().enumerate() {
                let matched = candidate.indices.binary_search(&(index as u32)).is_ok();
                let char_style = if matched {
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                spans.push(Span::styled(ch.to_string(), char_style));
            }
            spans.push(Span::styled(
                if skills_only {
                    format!("  {}", candidate.description)
                } else {
                    format!("  [{}] {}", candidate.kind.label(), candidate.description)
                },
                Style::default().fg(theme.muted),
            ));
            Line::from(spans)
        })
        .collect();
    if geometry.show_status
        && let Some((text, color)) = status
    {
        lines.push(Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(color),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(if skills_only {
            " Skills "
        } else {
            " Mentions "
        })
        .border_style(Style::default().fg(theme.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, geometry.area);
}

fn mention_popup_status(state: &AppState) -> Option<(String, Color)> {
    let candidates = &state.mention.candidates;
    if state.mention.sigil == Some(orca_runtime::mentions::MentionSigil::Dollar) {
        Some((
            if candidates.is_empty() {
                "No matching skills".to_string()
            } else {
                format!(
                    "{}/{} · ↑↓ select · PgUp/PgDn page · Home/End · Enter insert · Esc close",
                    state.mention.selected.saturating_add(1),
                    candidates.len()
                )
            },
            Color::DarkGray,
        ))
    } else {
        let phase = state.mention.phase.as_ref()?;
        mention_status_text(
            phase,
            state.mention.progress.scanned_paths,
            candidates.is_empty(),
        )
    }
}

fn mention_status_text(
    phase: &SearchPhase,
    scanned_paths: usize,
    candidates_empty: bool,
) -> Option<(String, Color)> {
    match phase {
        SearchPhase::Searching => Some(("Searching files…".to_string(), Color::DarkGray)),
        SearchPhase::Scanning => {
            Some((format!("Scanning… {scanned_paths} paths"), Color::DarkGray))
        }
        SearchPhase::Refreshing => Some(("Refreshing…".to_string(), Color::DarkGray)),
        SearchPhase::Complete if candidates_empty => {
            Some(("No matches".to_string(), Color::DarkGray))
        }
        SearchPhase::Complete => None,
        SearchPhase::Incomplete { .. } => Some(("Search incomplete".to_string(), Color::Red)),
        SearchPhase::Stopping => Some(("Stopping search…".to_string(), Color::DarkGray)),
    }
}

fn plan_approval_popup(area: Rect) -> Rect {
    let width = 78u16.min(area.width.saturating_sub(4));
    let height = 8u16.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.bottom().saturating_sub(height + 1),
        width,
        height,
    )
}

pub(crate) fn plan_approval_option_hit_index(
    state: &AppState,
    column: u16,
    row: u16,
) -> Option<usize> {
    state.plan_approval_dialog.as_ref()?;
    let popup = plan_approval_popup(state.frame_area?);
    if column <= popup.x || column + 1 >= popup.right() {
        return None;
    }
    let first_option_row = popup.y + 3;
    let index = row.checked_sub(first_option_row)? as usize;
    (index < 2).then_some(index)
}

fn render_plan_approval_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(dialog) = state.plan_approval_dialog.as_ref() else {
        return;
    };
    let popup = plan_approval_popup(frame.area());
    frame.render_widget(Clear, popup);

    let target_mode = state.pre_plan_approval_mode.unwrap_or_default().as_str();
    let options = [
        (
            "Yes, implement this plan",
            format!("Switch to {target_mode} and start coding."),
        ),
        (
            "No, stay in Plan mode",
            "Continue planning with feedback.".to_string(),
        ),
    ];
    let inner_width = popup.width.saturating_sub(4) as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            "  The plan is ready. Choose whether to start implementation.",
            Style::default().fg(theme.text),
        )),
        Line::from(""),
    ];
    for (index, (label, description)) in options.into_iter().enumerate() {
        let selected = index == dialog.selected;
        let marker = if selected { "▸ " } else { "  " };
        let label_style = if selected {
            Style::default()
                .fg(theme.plan_mode)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        let prefix = format!("{marker}{}. {label}", index + 1);
        let description_width = inner_width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
        lines.push(Line::from(vec![
            Span::styled(prefix, label_style),
            Span::styled(
                truncate_to_display_width(&format!("  {description}"), description_width),
                Style::default().fg(theme.muted),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ select · Enter confirm · PgUp/PgDn review plan · Esc stay in Plan mode",
        Style::default().fg(theme.muted),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Implement this plan? ")
        .border_style(Style::default().fg(theme.plan_mode));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Shared layout for the approval dialog, used by the renderer and the mouse
/// hit-test so option rows can never drift apart.
struct ApprovalDialogGeometry {
    popup: Rect,
    shown_diff_lines: usize,
    diff_truncated: bool,
    first_option_row: u16,
}

fn approval_dialog_geometry(
    area: Rect,
    dialog: &crate::types::ApprovalDialog,
) -> ApprovalDialogGeometry {
    let width = 64u16.min(area.width.saturating_sub(4));
    let max_height = area.height.saturating_sub(4).max(8);
    let fixed_content_rows = 3 + dialog.options.len() as u16 + 2 + u16::from(dialog.diff.is_some());
    let available_diff_rows = max_height
        .saturating_sub(2)
        .saturating_sub(fixed_content_rows) as usize;
    let source_diff_lines = dialog
        .diff
        .as_ref()
        .map(|diff| diff.lines().count())
        .unwrap_or(0);
    let desired_diff_lines = source_diff_lines.min(12);
    let diff_truncated =
        source_diff_lines > desired_diff_lines || desired_diff_lines > available_diff_rows;
    let truncation_row = usize::from(diff_truncated && available_diff_rows > 0);
    let shown_diff_lines =
        desired_diff_lines.min(available_diff_rows.saturating_sub(truncation_row));
    let height = (fixed_content_rows
        + shown_diff_lines as u16
        + u16::from(diff_truncated && available_diff_rows > 0)
        + 2)
    .min(max_height)
    .max(8);
    let popup = centered_rect(area, width, height);
    // Border, then tool/target/blank, then the bounded diff block.
    let first_option_row = popup.y
        + 1
        + 3
        + shown_diff_lines as u16
        + truncation_row as u16
        + u16::from(dialog.diff.is_some());
    ApprovalDialogGeometry {
        popup,
        shown_diff_lines,
        diff_truncated,
        first_option_row,
    }
}

/// Which approval option a click lands on, if any.
pub(crate) fn approval_option_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    let dialog = state.approval_dialog.as_ref()?;
    let area = state.frame_area?;
    let geometry = approval_dialog_geometry(area, dialog);
    let popup = geometry.popup;
    if popup.width < 3 || popup.height < 3 {
        return None;
    }
    if column <= popup.x || column + 1 >= popup.x + popup.width {
        return None;
    }
    if row + 1 >= popup.y + popup.height {
        return None;
    }
    let index = row.checked_sub(geometry.first_option_row)? as usize;
    (index < dialog.options.len()).then_some(index)
}

fn render_approval_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(dialog) = &state.approval_dialog else {
        return;
    };

    let area = frame.area();
    let geometry = approval_dialog_geometry(area, dialog);
    let popup_area = geometry.popup;
    let shown_diff_lines = geometry.shown_diff_lines;
    let diff_truncated = geometry.diff_truncated;
    let target_str = dialog.target.as_deref().unwrap_or("(none)");
    let inner_width = popup_area.width.saturating_sub(2) as usize;
    let target_str = truncate_to_display_width(target_str, inner_width.saturating_sub(9));

    // Build the diff/preview lines (colored) if a preview is present.
    let diff_lines: Vec<Line<'static>> = match &dialog.diff {
        Some(diff) => diff
            .lines()
            .take(shown_diff_lines)
            .map(|line| {
                let color = if line.starts_with('+') {
                    theme.diff_add
                } else if line.starts_with('-') {
                    theme.diff_remove
                } else if line.starts_with("@@") || line.starts_with('$') {
                    theme.border
                } else {
                    theme.muted
                };
                Line::from(Span::styled(
                    truncate_to_display_width(&format!("  {line}"), inner_width),
                    Style::default().fg(color),
                ))
            })
            .collect(),
        None => Vec::new(),
    };

    frame.render_widget(Clear, popup_area);

    let mut content: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("  tool   ", Style::default().fg(theme.muted)),
            Span::styled(
                dialog.tool.clone(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  target ", Style::default().fg(theme.muted)),
            Span::styled(target_str.clone(), Style::default().fg(theme.text)),
        ]),
        Line::from(""),
    ];

    content.extend(diff_lines);
    if diff_truncated {
        content.push(Line::from(Span::styled(
            "  … (preview truncated)",
            Style::default().fg(theme.muted),
        )));
    }
    if dialog.diff.is_some() {
        content.push(Line::from(""));
    }

    // The options, one per line, highlighted when selected.
    for (i, option) in dialog.options.iter().enumerate() {
        let selected = i == dialog.selected;
        let prefix = if selected { "▸ " } else { "  " };
        let key_color = match option {
            ApprovalOption::Deny => theme.error,
            _ => theme.success,
        };
        let label_style = if selected {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
        };
        let label_text = match option {
            ApprovalOption::AlwaysTool => format!("always allow \"{}\"", dialog.tool),
            ApprovalOption::AlwaysTarget => "always allow this exact call".to_string(),
            _ => option.label().to_string(),
        };
        let label_text = truncate_to_display_width(&label_text, inner_width.saturating_sub(8));
        content.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.border)),
            Span::styled(
                format!("[{}] ", option.key()),
                Style::default().fg(key_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label_text, label_style),
        ]));
    }

    content.push(Line::from(""));
    content.push(Line::from(Span::styled(
        "  ↑↓ select · Enter · 1/2/3/4 · legacy y/A/a/n",
        Style::default().fg(theme.muted),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(dialog.title())
        .border_style(Style::default().fg(theme.approval));

    let paragraph = Paragraph::new(content).block(block);
    frame.render_widget(paragraph, popup_area);
}

/// Which session (index into `session_picker_sessions`) a click lands on,
/// replicating `render_session_picker`'s line layout: three header lines,
/// then one title line per filtered session plus an optional metadata line.
/// Long wrapped titles can shift rows below them; the mapping is then off by
/// the wrapped amount, which degrades to selecting a neighbour.
pub(crate) fn session_picker_hit_index(state: &AppState, row: u16) -> Option<usize> {
    let area = state.frame_area?;
    if area.width < 3 || area.height < 3 {
        return None;
    }
    let inner_top = area.y + 1;
    let inner_bottom = area.y + area.height - 1;
    if row < inner_top + 3 || row >= inner_bottom {
        return None;
    }
    let mut current = inner_top + 3;
    for index in state.filtered_session_indices() {
        let session = &state.session_picker_sessions[index];
        let rows = 1 + u16::from(session_permission_metadata_label(session).is_some());
        if row >= current && row < current + rows {
            return Some(index);
        }
        current = current.saturating_add(rows);
        if current >= inner_bottom {
            break;
        }
    }
    None
}

/// Map a click inside the composer to a `(row, col)` cursor position in the
/// textarea, replicating `render_input`'s wrap and scroll behavior.
pub(crate) fn composer_click_target(
    textarea: &TextArea,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(u16, u16)> {
    let inner = textarea
        .block()
        .map(|block| block.inner(area))
        .unwrap_or(area);
    if inner.is_empty() || !inner.contains(ratatui::layout::Position::new(column, row)) {
        return None;
    }
    if textarea.is_empty() {
        return Some((0, 0));
    }

    let width = inner.width as usize;
    let layout = textarea_visual_layout(textarea, width);
    if layout.rows.is_empty() {
        return Some((0, 0));
    }

    // Same scroll-to-cursor behavior as render_input.
    let start = textarea_visible_start(&layout, inner.height as usize);
    let clicked = (start + (row - inner.y) as usize).min(layout.rows.len() - 1);
    let target = (column - inner.x) as usize;
    if clicked == layout.cursor_visual_row && target == layout.cursor_display_col {
        let (cursor_row, cursor_col) = textarea.cursor();
        return Some((
            cursor_row.min(u16::MAX as usize) as u16,
            cursor_col.min(u16::MAX as usize) as u16,
        ));
    }

    let visual_row = &layout.rows[clicked];
    let logical_row = visual_row.logical_row;

    // Walk display widths to find the character cell under the pointer.
    let mut acc = 0usize;
    let mut char_col = visual_row.logical_range.start;
    let mut leading_zero_width_start = None;
    for grapheme in &visual_row.graphemes {
        if grapheme.width == 0 {
            if acc == 0 {
                leading_zero_width_start.get_or_insert(grapheme.logical_range.start);
            }
            char_col = grapheme.logical_range.end;
            continue;
        }
        if grapheme.width > 0 && target < acc + grapheme.width {
            char_col = leading_zero_width_start.unwrap_or(grapheme.logical_range.start);
            break;
        }
        leading_zero_width_start = None;
        acc += grapheme.width;
        char_col = grapheme.logical_range.end;
    }
    Some((
        logical_row.min(u16::MAX as usize) as u16,
        char_col.min(u16::MAX as usize) as u16,
    ))
}

fn render_setup(frame: &mut Frame, state: &AppState, textarea: &TextArea, theme: &Theme) {
    let area = frame.area();

    match state.setup_step {
        0 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 16u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "   ___                ",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  / _ \\ _ __ ___ __ _ ",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    " | | | | '__/ __/ _` |",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    " | |_| | | | (_| (_| |",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  \\___/|_|  \\___\\__,_|",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  A DeepSeek-native coding agent",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Let's get you set up!",
                    Style::default().fg(Color::Green),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press Enter to continue...",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Welcome ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        1 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 14u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

            let inner =
                Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(Rect::new(
                    popup_area.x + 1,
                    popup_area.y + 1,
                    popup_area.width.saturating_sub(2),
                    popup_area.height.saturating_sub(2),
                ));

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Step 1: API Key",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Orca needs a DeepSeek API key to function.",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  https://platform.deepseek.com/api_keys",
                    Style::default().fg(Color::Blue),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Paste below and press Enter:",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Setup ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
            render_textarea_surface(frame, inner[1], textarea, None, None, theme, true);
        }
        2 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 12u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  ✓ API key saved successfully!",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Saved to: ~/.orca/auth.json",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  You're all set! Orca is ready to use.",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press Enter to start...",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Setup Complete ")
                .border_style(Style::default().fg(Color::Green));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        _ => {}
    }
}

struct PendingCodeBlock {
    language: Option<String>,
    source: String,
}

fn append_code_block(lines: &mut Vec<Line<'static>>, pending: PendingCodeBlock, theme: &Theme) {
    let highlighted = pending.language.as_deref().and_then(|language| {
        highlight_code(
            &pending.source,
            language,
            theme.syntax_theme,
            theme.color_level,
        )
    });

    if let Some(highlighted) = highlighted {
        for mut source_line in highlighted {
            source_line.insert(0, Span::raw("  "));
            lines.push(Line::from(source_line));
        }
    } else {
        let style = Style::default().fg(theme.muted);
        for source_line in pending.source.lines() {
            lines.push(Line::from(Span::styled(format!("  {source_line}"), style)));
        }
    }
}

fn render_markdown(input: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(input, opts);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default().fg(theme.text)];
    let mut pending_code_block: Option<PendingCodeBlock> = None;
    let mut list_depth: u16 = 0;

    // Table buffering state
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();

    for event in parser {
        if let Some(pending) = pending_code_block.as_mut() {
            match event {
                Event::Text(text) => pending.source.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let pending = pending_code_block
                        .take()
                        .expect("pending code block exists");
                    append_code_block(&mut lines, pending, theme);
                }
                _ => {}
            }
            continue;
        }

        // When inside a table, buffer content instead of rendering immediately
        if in_table {
            match event {
                Event::Start(Tag::TableHead) => {}
                Event::Start(Tag::TableRow) => {}
                Event::Start(Tag::TableCell) => {
                    current_cell.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    current_row.push(std::mem::take(&mut current_cell));
                }
                Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                    table_rows.push(std::mem::take(&mut current_row));
                }
                Event::End(TagEnd::Table) => {
                    render_table(&table_rows, &mut lines, width, theme);
                    table_rows.clear();
                    in_table = false;
                }
                Event::Text(text) => {
                    current_cell.push_str(&text);
                }
                Event::Code(code) => {
                    current_cell.push('`');
                    current_cell.push_str(&code);
                    current_cell.push('`');
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Table(_alignments)) => {
                flush_line(&mut current_spans, &mut lines);
                in_table = true;
                table_rows.clear();
            }
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let color = match level {
                        pulldown_cmark::HeadingLevel::H1 => theme.markdown_h1,
                        pulldown_cmark::HeadingLevel::H2 => theme.markdown_h2,
                        _ => theme.markdown_h3,
                    };
                    style_stack.push(Style::default().fg(color).add_modifier(Modifier::BOLD));
                }
                Tag::Strong => {
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut current_spans, &mut lines);
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => Some(info.into_string()),
                        CodeBlockKind::Indented => None,
                    };
                    pending_code_block = Some(PendingCodeBlock {
                        language,
                        source: String::new(),
                    });
                }
                Tag::List(_) => {
                    list_depth += 1;
                }
                Tag::Item => {
                    let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                    current_spans.push(Span::styled(
                        format!("{indent}• "),
                        Style::default().fg(theme.muted),
                    ));
                }
                Tag::BlockQuote(_) => {
                    current_spans.push(Span::styled("│ ", Style::default().fg(theme.muted)));
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.fg(theme.muted));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    style_stack.pop();
                }
                TagEnd::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                }
                TagEnd::Item => {
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::BlockQuote(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = *style_stack.last().unwrap_or(&Style::default());
                current_spans.push(Span::styled(text.to_string(), style));
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!("`{code}`"),
                    Style::default().fg(theme.markdown_inline_code),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            _ => {}
        }
    }

    flush_line(&mut current_spans, &mut lines);
    lines
}

fn render_table(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
    theme: &Theme,
) {
    if rows.is_empty() {
        return;
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

    let ideal_widths: Vec<usize> = (0..num_cols)
        .map(|col| {
            rows.iter()
                .map(|row| {
                    row.get(col)
                        .map(|c| UnicodeWidthStr::width(c.as_str()))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();

    let col_gap: usize = 2;
    let overhead = col_gap * (num_cols.saturating_sub(1));
    let ideal_total = ideal_widths.iter().sum::<usize>() + overhead;

    if ideal_total <= available_width {
        render_table_grid(rows, &ideal_widths, col_gap, lines, theme);
    } else {
        let col_widths = allocate_column_widths(&ideal_widths, available_width, col_gap);
        let max_col = col_widths.iter().copied().max().unwrap_or(0);
        if max_col < 12 && num_cols > 2 {
            render_table_as_records(rows, lines, available_width, theme);
        } else {
            render_table_grid(rows, &col_widths, col_gap, lines, theme);
        }
    }
    lines.push(Line::from(""));
}

fn allocate_column_widths(
    ideal_widths: &[usize],
    available_width: usize,
    col_gap: usize,
) -> Vec<usize> {
    let num_cols = ideal_widths.len();
    let overhead = col_gap * num_cols.saturating_sub(1);
    let usable = available_width.saturating_sub(overhead);

    let min_widths: Vec<usize> = ideal_widths.iter().map(|&w| w.min(6).max(3)).collect();
    let min_total: usize = min_widths.iter().sum();

    if usable <= min_total {
        return min_widths;
    }

    let ideal_total: usize = ideal_widths.iter().sum();
    if ideal_total <= usable {
        return ideal_widths.to_vec();
    }

    let mut widths = ideal_widths.to_vec();
    let mut excess = ideal_total - usable;

    while excess > 0 {
        let max_w = widths.iter().copied().max().unwrap_or(0);
        if max_w <= 6 {
            break;
        }
        let max_count = widths.iter().filter(|&&w| w == max_w).count();
        let second_max = widths
            .iter()
            .copied()
            .filter(|&w| w < max_w)
            .max()
            .unwrap_or(6);
        let shrink_each = (max_w - second_max).min((excess + max_count - 1) / max_count);
        for w in &mut widths {
            if *w == max_w {
                let s = shrink_each.min(excess);
                *w -= s;
                excess -= s;
                if excess == 0 {
                    break;
                }
            }
        }
    }

    for (w, &min_w) in widths.iter_mut().zip(min_widths.iter()) {
        *w = (*w).max(min_w);
    }
    widths
}

fn render_table_grid(
    rows: &[Vec<String>],
    col_widths: &[usize],
    col_gap: usize,
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
) {
    let header_style = Style::default()
        .fg(theme.markdown_h1)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(theme.text);
    let separator_style = Style::default().fg(theme.muted);
    let gap_str: String = " ".repeat(col_gap);

    for (row_idx, row) in rows.iter().enumerate() {
        let wrapped_cells: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = col_widths.get(i).copied().unwrap_or(6);
                wrap_text(cell, w)
            })
            .collect();

        let max_lines = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        let style = if row_idx == 0 {
            header_style
        } else {
            cell_style
        };

        for line_idx in 0..max_lines {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (col_idx, wrapped) in wrapped_cells.iter().enumerate() {
                let w = col_widths.get(col_idx).copied().unwrap_or(6);
                let text = wrapped.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                let display_width = UnicodeWidthStr::width(text);
                let padding = w.saturating_sub(display_width);
                spans.push(Span::styled(
                    format!("{text}{}", " ".repeat(padding)),
                    style,
                ));
                if col_idx < col_widths.len() - 1 {
                    spans.push(Span::styled(gap_str.clone(), separator_style));
                }
            }
            lines.push(Line::from(spans));
        }

        if row_idx == 0 {
            let sep: String = col_widths
                .iter()
                .enumerate()
                .map(|(i, &w)| {
                    let seg = "━".repeat(w);
                    if i < col_widths.len() - 1 {
                        format!("{seg}{}", " ".repeat(col_gap))
                    } else {
                        seg
                    }
                })
                .collect();
            lines.push(Line::from(Span::styled(sep, separator_style)));
        }
    }
}

fn render_table_as_records(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
    theme: &Theme,
) {
    let header_style = Style::default()
        .fg(theme.markdown_h1)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.markdown_h3);
    let value_style = Style::default().fg(theme.text);
    let separator_style = Style::default().fg(theme.muted);

    let headers: Vec<&str> = rows
        .first()
        .map(|r| r.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let max_key_width = headers
        .iter()
        .map(|h| UnicodeWidthStr::width(*h))
        .max()
        .unwrap_or(0);

    let value_indent = max_key_width + 3;
    let value_width = available_width.saturating_sub(value_indent).max(10);

    for (row_idx, row) in rows.iter().enumerate().skip(1) {
        let record_label = format!("─── Record {} ", row_idx);
        let fill = "─"
            .repeat(available_width.saturating_sub(UnicodeWidthStr::width(record_label.as_str())));
        lines.push(Line::from(vec![
            Span::styled(record_label, separator_style),
            Span::styled(fill, separator_style),
        ]));

        for (col_idx, cell) in row.iter().enumerate() {
            let key = headers.get(col_idx).copied().unwrap_or("?");
            let key_pad = max_key_width.saturating_sub(UnicodeWidthStr::width(key));

            let wrapped_value = wrap_text(cell, value_width);
            if let Some(first_line) = wrapped_value.first() {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}{}: ", " ".repeat(key_pad), key), key_style),
                    Span::styled(first_line.clone(), value_style),
                ]));
            }
            for extra_line in wrapped_value.iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::styled(" ".repeat(value_indent).to_string(), value_style),
                    Span::styled(extra_line.clone(), value_style),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    if !headers.is_empty() && rows.len() > 1 {
        let header_line = headers.join(" │ ");
        lines.insert(
            lines.len().saturating_sub(rows.len()), // insert near the top section
            Line::from(Span::styled(
                format!("Columns: {header_line}"),
                header_style,
            )),
        );
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let text_width = UnicodeWidthStr::width(text);
    if text_width <= width {
        return vec![text.to_string()];
    }

    let mut result: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for word in text.split_inclusive(|c: char| c.is_whitespace() || c == '/' || c == '-') {
        let word_width = UnicodeWidthStr::width(word);
        if current_width + word_width <= width || current_line.is_empty() {
            current_line.push_str(word);
            current_width += word_width;
        } else {
            result.push(current_line.trim_end().to_string());
            current_line = word.to_string();
            current_width = word_width;
        }
    }
    if !current_line.is_empty() {
        result.push(current_line.trim_end().to_string());
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        lines.join(" ")
    } else {
        let joined: String = lines[..max_lines].join(" ");
        format!("{joined}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        PlanApprovalDialog, SlashMenu, SlashMenuItem, SurfaceProjectionState, TuiEvent,
        TuiInteractionKey, TuiInteractionKind,
    };
    use chrono::Utc;
    use crossbeam_channel as mpsc;
    use orca_core::config::{AdditionalWorkingDirectory, ThemeName};
    use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};
    use orca_core::plan_types::{PlanItem, PlanStatus};
    use orca_runtime::history::SessionSummary;
    use ratatui::backend::Backend;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CursorEvent {
        Show,
        Hide,
        Move(Position),
    }

    struct RecordingBackend {
        inner: ratatui::backend::TestBackend,
        events: Arc<Mutex<Vec<CursorEvent>>>,
    }

    impl RecordingBackend {
        fn new(width: u16, height: u16) -> (Self, Arc<Mutex<Vec<CursorEvent>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    inner: ratatui::backend::TestBackend::new(width, height),
                    events: Arc::clone(&events),
                },
                events,
            )
        }
    }

    impl Backend for RecordingBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.events.lock().unwrap().push(CursorEvent::Hide);
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.events.lock().unwrap().push(CursorEvent::Show);
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> std::io::Result<Position> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
            let position = position.into();
            self.events
                .lock()
                .unwrap()
                .push(CursorEvent::Move(position));
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn append_lines(&mut self, line_count: u16) -> std::io::Result<()> {
            self.inner.append_lines(line_count)
        }

        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.size()
        }

        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }

        fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> std::io::Result<()> {
            self.inner.scroll_region_up(region, line_count)
        }

        fn scroll_region_down(
            &mut self,
            region: Range<u16>,
            line_count: u16,
        ) -> std::io::Result<()> {
            self.inner.scroll_region_down(region, line_count)
        }
    }

    fn take_cursor_events(events: &Arc<Mutex<Vec<CursorEvent>>>) -> Vec<CursorEvent> {
        std::mem::take(&mut *events.lock().unwrap())
    }

    fn foregrounds(line: &Line<'static>) -> HashSet<Color> {
        line.spans.iter().filter_map(|span| span.style.fg).collect()
    }

    fn rendered_text(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    const REFINED_TOOL_DIFF: &str = "\
--- a/item.py
+++ b/item.py
@@ -1,2 +1,2 @@
-value = 1
+value = 2
 print(value)
";

    fn refined_tool_message() -> ChatMessage {
        ChatMessage::ToolCall {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            target: Some("item.py".to_string()),
            status: "completed".to_string(),
            output: None,
            diff: Some(REFINED_TOOL_DIFF.to_string()),
            kind: None,
            expanded: false,
        }
    }

    fn queued(text: &str) -> crate::queued_input::QueuedUserMessage {
        crate::queued_input::QueuedUserMessage::from_composer(
            text.to_string(),
            Vec::new(),
            orca_runtime::mentions::MentionBindings::default(),
        )
        .unwrap()
    }

    #[test]
    fn queued_preview_uses_two_three_and_exactly_three_rows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for (count, expected_rows) in [(1, 2), (2, 3), (3, 3), (10, 3), (64, 3)] {
            let mut state = test_state();
            for index in 0..count {
                state
                    .enqueue_user_message(queued(&format!("item {index}")))
                    .unwrap();
            }
            let lines = queued_preview_lines(&state, 80, &theme);
            assert_eq!(lines.len(), expected_rows, "count={count}");
            assert!(lines[0].to_string().contains(&format!("Queued {count}")));
            assert!(lines[1].to_string().contains("item 0"));
            if count > 2 {
                assert!(
                    lines[2]
                        .to_string()
                        .contains(&format!("item {}", count - 1))
                );
            }
        }
    }

    #[test]
    fn queued_preview_projects_exact_error_header_and_style() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.report_queued_input_error("follow-up action queue is full".to_string());

        let lines = queued_preview_lines(&state, 80, &theme);

        assert_eq!(
            lines[0].to_string(),
            " Queue error · follow-up action queue is full"
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.error));
    }

    #[test]
    fn queued_preview_keeps_unicode_clusters_and_paste_placeholders() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        let visible = "e\u{301} 👍🏽 👨‍👩‍👧‍👦 1️⃣ 中文 [Pasted Content 1001 chars]";
        state
            .enqueue_user_message(
                crate::queued_input::QueuedUserMessage::from_composer(
                    visible.to_string(),
                    vec![(
                        "[Pasted Content 1001 chars]".to_string(),
                        "secret payload".repeat(100),
                    )],
                    orca_runtime::mentions::MentionBindings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        let rendered = queued_preview_lines(&state, 80, &theme)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for cluster in ["e\u{301}", "👍🏽", "👨‍👩‍👧‍👦", "1️⃣", "中文"] {
            assert!(rendered.contains(cluster), "{cluster:?}: {rendered:?}");
        }
        assert!(rendered.contains("[Pasted Content 1001 chars]"));
        assert!(!rendered.contains("secret payload"));
    }

    #[test]
    fn queued_preview_is_hidden_outside_conversation_idle_or_running() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.enqueue_user_message(queued("queued")).unwrap();
        for (status, panel, visible) in [
            (AppStatus::Idle, PanelMode::Conversation, true),
            (AppStatus::Running, PanelMode::Conversation, true),
            (AppStatus::WaitingUserInput, PanelMode::Conversation, false),
            (AppStatus::WaitingApproval, PanelMode::Conversation, false),
            (AppStatus::Compacting, PanelMode::Conversation, false),
            (AppStatus::Idle, PanelMode::Workflows, false),
            (AppStatus::Idle, PanelMode::Agents, false),
        ] {
            state.status = status;
            state.panel_mode = panel;
            assert_eq!(
                !queued_preview_lines(&state, 80, &theme).is_empty(),
                visible,
                "{status:?} {panel:?}"
            );
        }
    }

    #[test]
    fn queued_preview_never_overlaps_search_composer_status_or_cursor() {
        let mut state = test_state();
        state.enter_running();
        state
            .enqueue_user_message(queued("queued follow up"))
            .unwrap();
        state.open_transcript_search();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::from(["draft"]);
        let (backend, events) = RecordingBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        let rendered = format!("{:?}", terminal.backend().inner.buffer());
        assert!(rendered.contains("Queued 1"));
        let search = state.search_area.unwrap();
        let input = state.input_area.unwrap();
        assert!(search.bottom() <= input.y);
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert!(search.contains(cursor));
        assert!(
            take_cursor_events(&events)
                .iter()
                .any(|event| matches!(event, CursorEvent::Move(_)))
        );
    }

    #[test]
    fn compact_queued_preview_frames_never_panic_or_escape_bounds() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for width in [0, 1, 2, 8] {
            for height in 1..=8 {
                let mut state = test_state();
                state.enter_running();
                state.enqueue_user_message(queued("queued")).unwrap();
                let textarea = TextArea::from(["draft"]);
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();

                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .unwrap();

                if let Some(input) = state.input_area {
                    assert!(input.bottom() <= height, "{width}x{height}: {input:?}");
                }
                assert_eq!(terminal.backend().buffer().area.width, width);
                assert_eq!(terminal.backend().buffer().area.height, height);
            }
        }
    }

    fn line_containing<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Line<'static> {
        let (marker, source) = needle
            .strip_prefix('+')
            .map(|source| (Some('+'), source))
            .or_else(|| needle.strip_prefix('-').map(|source| (Some('-'), source)))
            .unwrap_or((None, needle));
        lines
            .iter()
            .find(|line| {
                line.to_string().contains(source)
                    && marker.is_none_or(|marker| {
                        line.spans
                            .first()
                            .is_some_and(|span| span.content.ends_with(&format!("{marker} ")))
                    })
            })
            .unwrap_or_else(|| panic!("rendered line containing {needle:?}"))
    }

    #[test]
    fn tool_message_uses_refined_new_side_styles_but_keeps_delete_hunk_styles() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let tool = refined_tool_message();
        let refined = crate::diff_highlight::RefinedDiffStyles::from([
            (
                1,
                vec![Span::styled(
                    "value = 2",
                    Style::default().fg(Color::Magenta),
                )],
            ),
            (
                2,
                vec![Span::styled(
                    "print(value)",
                    Style::default().fg(Color::Cyan),
                )],
            ),
        ]);
        let cold = build_lines_for_message(&tool, &theme, 80, 0, false, None);
        let warm = build_lines_for_message(&tool, &theme, 80, 0, false, Some(&refined));

        let cold_delete = line_containing(&cold, "-value = 1");
        let warm_delete = line_containing(&warm, "-value = 1");
        let warm_insert = line_containing(&warm, "+value = 2");
        let warm_context = line_containing(&warm, "print(value)");

        assert_eq!(warm_insert.spans[0].content.as_ref(), "  1 + ");
        assert_eq!(warm_insert.spans[1].style.fg, Some(Color::Magenta));
        assert_eq!(warm_context.spans[0].content.as_ref(), "2 2   ");
        assert_eq!(warm_context.spans[1].style.fg, Some(Color::Cyan));
        assert_eq!(cold_delete.spans, warm_delete.spans);
    }

    #[test]
    fn tool_message_rejects_mismatched_refined_text() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let tool = refined_tool_message();
        let refined = crate::diff_highlight::RefinedDiffStyles::from([(
            2,
            vec![Span::styled(
                "different text",
                Style::default().fg(Color::Magenta),
            )],
        )]);
        let cold = build_lines_for_message(&tool, &theme, 80, 0, false, None);
        let warm = build_lines_for_message(&tool, &theme, 80, 0, false, Some(&refined));

        assert_eq!(
            line_containing(&cold, "print(value)").spans,
            line_containing(&warm, "print(value)").spans
        );
    }

    #[test]
    fn single_message_none_and_non_tool_refined_rendering_keep_existing_behavior() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let tool = refined_tool_message();
        let assistant = ChatMessage::Assistant("plain response".to_string());
        let irrelevant = crate::diff_highlight::RefinedDiffStyles::from([(
            1,
            vec![Span::styled(
                "plain response",
                Style::default().fg(Color::Magenta),
            )],
        )]);

        assert_eq!(
            build_lines_for_message(&tool, &theme, 80, 0, false, None),
            build_lines_for_messages(std::slice::from_ref(&tool), &theme, 80, 0, false)
        );
        assert_eq!(
            build_lines_for_message(&assistant, &theme, 80, 0, false, Some(&irrelevant)),
            build_lines_for_message(&assistant, &theme, 80, 0, false, None)
        );
    }

    #[test]
    fn search_overlay_styles_only_visible_matches_and_selection_wins() {
        use crate::selection::{SelectionGranularity, SelectionPos, TranscriptSelection};
        use crate::transcript_search::{
            TranscriptLineIdentity, TranscriptSearchMatch, TranscriptSearchState,
        };

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let lines = vec![Line::from("alpha beta"), Line::from("tail")];
        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("alpha");
        search.refresh_with(1, 0, |_| {
            vec![
                TranscriptSearchMatch::new(
                    SelectionPos { row: 0, col: 0 },
                    SelectionPos { row: 0, col: 5 },
                    TranscriptLineIdentity {
                        message_revision: 1,
                        line_index: 0,
                    },
                    0..5,
                ),
                TranscriptSearchMatch::new(
                    SelectionPos { row: 100, col: 0 },
                    SelectionPos { row: 100, col: 4 },
                    TranscriptLineIdentity {
                        message_revision: 2,
                        line_index: 0,
                    },
                    0..4,
                ),
            ]
        });
        let mut selection = TranscriptSelection::unit(
            SelectionGranularity::Cell,
            SelectionPos { row: 0, col: 1 },
            SelectionPos { row: 0, col: 2 },
        );
        selection.dragging = false;

        let overlaid = apply_transcript_overlays(lines, &search, Some(selection), 0, &theme);
        assert_eq!(search.visible_matches(0, 2).count(), 1);
        assert!(
            overlaid[0]
                .spans
                .iter()
                .any(|span| { span.style.bg == theme.search_match_active_style().bg })
        );
        assert!(
            overlaid[0]
                .spans
                .iter()
                .any(|span| { span.style.bg == theme.selection_style().bg })
        );
    }

    #[test]
    fn visible_match_iterator_bounds_overlay_work_to_viewport() {
        use crate::selection::SelectionPos;
        use crate::transcript_search::{
            TranscriptLineIdentity, TranscriptSearchMatch, TranscriptSearchState,
        };

        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("needle");
        search.refresh_with(1, 0, |_| {
            (0..10_001)
                .map(|row| {
                    TranscriptSearchMatch::new(
                        SelectionPos { row, col: 0 },
                        SelectionPos { row, col: 6 },
                        TranscriptLineIdentity {
                            message_revision: row as u64 + 1,
                            line_index: 0,
                        },
                        0..6,
                    )
                })
                .collect()
        });

        assert_eq!(search.visible_matches(5_000, 5_020).count(), 20);
    }

    #[test]
    fn open_search_reserves_one_row_without_squeezing_composer_or_status() {
        let area = Rect::new(0, 0, 80, 20);
        let chunks = main_layout(area, 0, 0, 2, 0, 1, 3);
        assert_eq!(chunks[5].height, 1);
        assert_eq!(chunks[6].height, 3);
        assert_eq!(chunks[7].height, 1);
        assert_eq!(chunks[5].bottom(), chunks[6].y);
    }

    #[test]
    fn compact_search_layout_preserves_fixed_chrome_before_transcript() {
        let chunks = main_layout(Rect::new(0, 0, 20, 6), 0, 0, 0, 0, 1, 3);
        assert_eq!(chunks[1].height, 1);
        assert_eq!(chunks[5].height, 1);
        assert_eq!(chunks[6].height, 3);
        assert_eq!(chunks[7].height, 1);
    }

    #[test]
    fn queue_preview_yields_before_search_composer_and_status() {
        let area = Rect::new(0, 0, 20, 5);
        let without_queue = main_layout(area, 0, 0, 0, 0, 1, 3);
        let with_queue = main_layout(area, 0, 0, 0, 3, 1, 3);

        assert_eq!(with_queue[4].height, 0);
        assert_eq!(with_queue[5].height, without_queue[5].height);
        assert_eq!(with_queue[6].height, without_queue[6].height);
        assert_eq!(with_queue[7].height, without_queue[7].height);
    }

    #[test]
    fn search_frame_shows_query_count_and_hardware_cursor() {
        let mut state = test_state();
        state.push_message(ChatMessage::Assistant("alpha beta alpha".to_string()));
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Find:"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("1/2"));
        let cursor = terminal.get_cursor_position().expect("hardware cursor");
        assert!(state.search_area.expect("search area").contains(cursor));
    }

    #[test]
    fn search_frame_status_matrix_and_zero_counts_are_stable() {
        for status in [
            AppStatus::Idle,
            AppStatus::Running,
            AppStatus::WaitingUserInput,
        ] {
            let mut state = test_state();
            state.set_status(status);
            state.push_message(ChatMessage::System("alpha".to_string()));
            state.open_transcript_search();
            let theme = Theme::named(orca_core::config::ThemeName::Dark);
            let textarea = TextArea::default();
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(24, 8))
                .expect("test backend");

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("empty query draw");
            let empty = format!("{:?}", terminal.backend().buffer());
            assert!(empty.contains("0/0"), "{status:?}: {empty}");

            state.replace_transcript_search_query("missing");
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("missing query draw");
            let missing = format!("{:?}", terminal.backend().buffer());
            assert!(missing.contains("0/0"), "{status:?}: {missing}");
            assert!(state.search_area.is_some());
        }
    }

    #[test]
    fn narrow_search_frame_keeps_count_and_cursor_segment_without_composer_cursor() {
        let mut state = test_state();
        state.push_message(ChatMessage::System(
            "long-query-tail long-query-tail".to_string(),
        ));
        state.open_transcript_search();
        state.replace_transcript_search_query("prefix-long-query-tail");
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::from(["COMPOSER_CURSOR_SENTINEL"]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(18, 8))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("narrow draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("0/0"));
        assert!(rendered.contains("tail"));
        let cursor = terminal.get_cursor_position().expect("search cursor");
        assert!(state.search_area.unwrap().contains(cursor));
        assert!(!state.input_area.unwrap().contains(cursor));
    }

    #[test]
    fn shortcuts_and_approval_hide_search_hardware_cursor() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut shortcuts = test_state();
        shortcuts.open_transcript_search();
        shortcuts.show_shortcuts = true;
        let (backend, events) = RecordingBackend::new(50, 12);
        let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut shortcuts, &textarea, &theme))
            .expect("shortcuts draw");
        assert_eq!(take_cursor_events(&events), [CursorEvent::Hide]);

        let mut approval = test_state();
        approval.open_transcript_search();
        approval.set_status(AppStatus::WaitingApproval);
        let (backend, events) = RecordingBackend::new(50, 12);
        let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut approval, &textarea, &theme))
            .expect("approval draw");
        assert_eq!(take_cursor_events(&events), [CursorEvent::Hide]);
    }

    #[test]
    fn adjacent_assistant_chunks_preserve_only_source_blank_rows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let first = ChatMessage::AssistantChunk {
            text: "first paragraph\n\n".to_string(),
            trailing_blank: true,
        };
        let second = ChatMessage::AssistantChunk {
            text: "```rust\nfn main() {}\n```\n".to_string(),
            trailing_blank: false,
        };
        let tail = ChatMessage::Assistant("tail".to_string());

        let first_lines = build_lines_for_message(&first, &theme, 80, 0, false, None);
        let second_lines = build_lines_for_message(&second, &theme, 80, 0, false, None);
        let tail_lines = build_lines_for_message(&tail, &theme, 80, 0, false, None);

        assert_eq!(
            first_lines.last().map(ToString::to_string),
            Some(String::new())
        );
        assert_ne!(
            second_lines.last().map(ToString::to_string),
            Some(String::new())
        );
        assert_eq!(
            tail_lines.last().map(ToString::to_string),
            Some(String::new())
        );
    }

    #[test]
    fn frozen_fenced_code_preserves_syntax_foregrounds() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let chunk = ChatMessage::AssistantChunk {
            text: "```rust\nfn main() { let value = 1; }\n```\n".to_string(),
            trailing_blank: false,
        };

        let lines = build_lines_for_message(&chunk, &theme, 80, 0, false, None);
        let code_line = lines
            .iter()
            .find(|line| line.to_string().contains("fn main"))
            .expect("frozen fenced source");

        assert!(foregrounds(code_line).len() >= 2);
    }

    #[test]
    fn fenced_rust_code_preserves_text_and_uses_token_foregrounds() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let input = "```rust,no_run\nfn main() { let message = \"hello\"; }\n```";

        let lines = render_markdown(input, 80, &theme);
        let code_line = lines
            .iter()
            .find(|line| line.to_string().contains("fn main"))
            .expect("rendered Rust source");

        assert_eq!(
            code_line.to_string(),
            "  fn main() { let message = \"hello\"; }"
        );
        assert!(foregrounds(code_line).len() >= 2);
    }

    #[test]
    fn unknown_and_oversized_fences_use_muted_theme_fallback() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let unknown = render_markdown("```not-a-real-language\nunknown_call();\n```", 80, &theme);
        let unknown_line = unknown
            .iter()
            .find(|line| line.to_string().contains("unknown_call"))
            .expect("unknown-language source");

        assert_eq!(unknown_line.to_string(), "  unknown_call();");
        assert!(
            unknown_line
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.muted))
        );

        let source = "x".repeat(crate::syntax_highlight::MAX_HIGHLIGHT_BYTES + 1);
        let oversized = render_markdown(&format!("```rust\n{source}\n```"), usize::MAX, &theme);
        let oversized_line = oversized
            .iter()
            .find(|line| line.to_string().len() > source.len())
            .expect("oversized Rust source");

        assert_eq!(oversized_line.to_string(), format!("  {source}"));
        assert!(
            oversized_line
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.muted))
        );
    }

    #[test]
    fn proposed_plan_keeps_markdown_code_span_styles() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut lines = Vec::new();

        append_proposed_plan_lines(
            &mut lines,
            "```rust\nfn main() { let message = \"hello\"; }\n```",
            80,
            &theme,
        );

        let code_line = lines
            .iter()
            .find(|line| line.to_string().contains("fn main"))
            .expect("prefixed Rust source");
        assert_eq!(
            code_line.to_string(),
            "    fn main() { let message = \"hello\"; }"
        );
        let markdown_foregrounds = code_line
            .spans
            .iter()
            .skip(1)
            .filter_map(|span| span.style.fg)
            .collect::<HashSet<_>>();
        assert!(markdown_foregrounds.len() >= 2);
    }

    #[test]
    fn multiline_rust_code_preserves_state_across_text_events() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let input = "```rust\r\n/* comment starts\r\n\r\ncomment continues */\r\nlet message = r#\"first\r\nsecond\"#;\r\n```";
        let mut in_code_block = false;
        let mut code_text_events = 0;
        for event in Parser::new_ext(input, Options::empty()) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
                Event::Text(_) if in_code_block => code_text_events += 1,
                Event::End(TagEnd::CodeBlock) => in_code_block = false,
                _ => {}
            }
        }
        assert!(
            code_text_events >= 2,
            "fixture must exercise multiple code-block Text events"
        );

        let lines = render_markdown(input, 80, &theme);
        let text = rendered_text(&lines);

        assert_eq!(
            text,
            vec![
                "  /* comment starts",
                "  ",
                "  comment continues */",
                "  let message = r#\"first",
                "  second\"#;",
            ]
        );
        let opening_comment = lines[0]
            .spans
            .iter()
            .find(|span| span.content.contains("comment starts"))
            .expect("opening comment span")
            .style
            .fg;
        let continued_comment = lines[2]
            .spans
            .iter()
            .find(|span| span.content.contains("comment continues"))
            .expect("continued comment span")
            .style
            .fg;
        assert_eq!(continued_comment, opening_comment);
        let opening_string = lines[3]
            .spans
            .iter()
            .find(|span| span.content.contains("first"))
            .expect("opening raw string span")
            .style
            .fg;
        let continued_string = lines[4]
            .spans
            .iter()
            .find(|span| span.content.contains("second"))
            .expect("continued raw string span")
            .style
            .fg;
        assert_eq!(continued_string, opening_string);
        assert_ne!(continued_string, continued_comment);
    }

    #[test]
    fn gray_code_fallback_preserves_source_boundaries() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let cases = [
            (
                "unknown empty",
                Some("not-a-real-language"),
                "",
                Vec::<&str>::new(),
            ),
            ("indented empty", None, "", Vec::new()),
            (
                "unknown internal blank",
                Some("not-a-real-language"),
                "alpha\n\nbeta\n",
                vec!["  alpha", "  ", "  beta"],
            ),
            (
                "indented leading indentation",
                None,
                "  alpha\n",
                vec!["    alpha"],
            ),
            (
                "unknown CRLF endings",
                Some("not-a-real-language"),
                "alpha\r\nbeta\r\n",
                vec!["  alpha", "  beta"],
            ),
            (
                "indented terminal newline",
                None,
                "alpha\nbeta\n",
                vec!["  alpha", "  beta"],
            ),
        ];

        for (name, language, source, expected) in cases {
            let mut lines = Vec::new();
            append_code_block(
                &mut lines,
                PendingCodeBlock {
                    language: language.map(str::to_owned),
                    source: source.to_owned(),
                },
                &theme,
            );

            assert_eq!(rendered_text(&lines), expected, "{name}");
            for line in &lines {
                let text = line.to_string();
                let source_text = text.strip_prefix("  ").expect("code indentation");
                if !source_text.is_empty() {
                    assert!(
                        line.spans
                            .iter()
                            .all(|span| span.style.fg == Some(theme.muted)),
                        "{name}: non-empty fallback content must use the muted theme color"
                    );
                }
            }
        }
    }

    #[test]
    fn inline_code_uses_the_selected_markdown_theme_color() {
        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            let theme = Theme::named(name);
            let lines = render_markdown("Use `cargo test` now.", 80, &theme);
            let inline = lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "`cargo test`")
                .expect("inline code span");

            assert_eq!(
                inline.style.fg,
                Some(theme.markdown_inline_code),
                "{name:?}"
            );
        }
    }

    #[test]
    fn markdown_roles_use_selected_theme_semantics() {
        let fixture = "# One\n## Two\n### Three\n\nPlain **bold** *italic*.\n\n- item\n\n> quote";

        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            let theme = Theme::named(name);
            let lines = render_markdown(fixture, 80, &theme);

            let span = |text: &str| {
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .find(|span| span.content == text)
                    .expect(text)
            };
            assert_eq!(span("One").style.fg, Some(theme.markdown_h1), "{name:?}");
            assert_eq!(span("Two").style.fg, Some(theme.markdown_h2), "{name:?}");
            assert_eq!(span("Three").style.fg, Some(theme.markdown_h3), "{name:?}");
            assert_eq!(span("Plain ").style.fg, Some(theme.text), "{name:?}");
            assert_eq!(span("bold").style.fg, Some(theme.text), "{name:?}");
            assert_eq!(span("italic").style.fg, Some(theme.text), "{name:?}");
            assert_eq!(span("• ").style.fg, Some(theme.muted), "{name:?}");
            assert_eq!(span("│ ").style.fg, Some(theme.muted), "{name:?}");
            assert_eq!(span("quote").style.fg, Some(theme.muted), "{name:?}");
        }
    }

    #[test]
    fn markdown_tables_and_plain_code_use_theme_semantics() {
        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            let theme = Theme::named(name);
            let grid = render_markdown("| Name | Value |\n|---|---|\n| A | B |", 80, &theme);
            let header = grid
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains("Name"))
                .expect("table header");
            let value = grid
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.trim() == "A")
                .expect("table value");
            let separator = grid
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains('━'))
                .expect("table separator");
            assert_eq!(header.style.fg, Some(theme.markdown_h1), "{name:?}");
            assert_eq!(value.style.fg, Some(theme.text), "{name:?}");
            assert_eq!(separator.style.fg, Some(theme.muted), "{name:?}");

            let records = render_markdown(
                "| First column | Second column | Third column |\n|---|---|---|\n| one | two | three |",
                18,
                &theme,
            );
            let key = records
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains("First column:"))
                .expect("record key");
            assert_eq!(key.style.fg, Some(theme.markdown_h3), "{name:?}");

            let plain = render_markdown("```not-a-real-language\ncall();\n```", 80, &theme);
            let source = plain
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains("call();"))
                .expect("plain code");
            assert_eq!(source.style.fg, Some(theme.muted), "{name:?}");
        }
    }

    #[test]
    fn welcome_lines_use_configured_app_version() {
        let (tx, _rx) = mpsc::unbounded();
        let state = AppState::new(
            tx,
            "9.8.7-test".to_string(),
            "deepseek-v4-pro".to_string(),
            "/tmp/project".to_string(),
        );
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let rendered = build_welcome_lines(&state, &theme)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("v9.8.7-test"));
    }

    #[test]
    fn terminal_tool_rows_render_interrupted_and_state_unknown_labels() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let messages = [
            ChatMessage::ToolCall {
                id: "cancelled".to_string(),
                name: "bash".to_string(),
                target: None,
                status: "cancelled".to_string(),
                output: Some("turn interrupted".to_string()),
                diff: None,
                kind: Some("cancelled".to_string()),
                expanded: false,
            },
            ChatMessage::ToolCall {
                id: "indeterminate".to_string(),
                name: "deploy".to_string(),
                target: None,
                status: "indeterminate".to_string(),
                output: Some("inspect external state".to_string()),
                diff: None,
                kind: Some("indeterminate".to_string()),
                expanded: false,
            },
        ];

        let rendered = build_lines_for_messages(&messages, &theme, 100, 0, false)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();

        assert!(rendered.contains("(interrupted)"));
        assert!(rendered.contains("(state unknown)"));
        assert!(!rendered.contains("(completed)"));
    }

    fn test_state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0".to_string(),
            "deepseek".to_string(),
            "/tmp".to_string(),
        )
    }

    fn monochrome_theme() -> Theme {
        Theme::resolve(
            ThemeName::Dark,
            crate::terminal_capabilities::TerminalProfile {
                background: crate::terminal_capabilities::TerminalBackground::Unknown,
                color_level: crate::terminal_capabilities::TerminalColorLevel::Monochrome,
            },
        )
    }

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(
            orca_core::cancel::OperationIdAllocator::new().allocate(),
            id,
            kind,
        )
    }

    fn goal_with_elapsed(status: ThreadGoalStatus, time_used_seconds: i64) -> ThreadGoal {
        ThreadGoal {
            session_id: "goal-session".to_string(),
            objective: "finish the migration".to_string(),
            status,
            token_budget: None,
            tokens_used: 42,
            time_used_seconds,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn long_goal_objective_keeps_status_visible_on_one_line() {
        let mut state = test_state();
        let mut goal = goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60);
        goal.objective = "将当前项目重构为分层清晰的生产级 Agent SDK monorepo".repeat(8);
        state.current_goal = Some(goal);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let width = 80u16;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 3))
            .expect("test backend");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_goal_banner(frame, area, &state, &theme);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let content_row = (0..width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(content_row.contains('…'));
        assert!(content_row.contains("● active"));
        assert!(content_row.contains("auto-continue"));
        assert!(!content_row.contains("monorepomonorepo"));
    }

    fn session_summary(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: "/workspace/project".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: "/tmp/session.jsonl".into(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    #[test]
    fn session_picker_labels_additional_directories_under_runtime_workspace_roots() {
        let mut state = test_state();
        state.status = AppStatus::SessionPicker;
        let mut session = session_summary("session-1", "workspace permissions");
        session.runtime_workspace_roots = vec!["/workspace/project".into()];
        session.additional_working_directories = vec![AdditionalWorkingDirectory::new(
            "/workspace/project/docs",
            "session",
        )];
        state.session_picker_sessions = vec![session];

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains(":workspace_roots/docs"));
        assert!(rendered.contains("session"));
    }

    #[test]
    fn current_session_picker_actions_hide_destructive_commands() {
        let mut state = test_state();
        state.status = AppStatus::SessionPicker;
        state.current_session_id = Some("session-1".to_string());
        state.session_picker_sessions = vec![session_summary("session-1", "Current session")];
        state.session_picker_phase = SessionPickerPhase::Actions {
            session_id: "session-1".to_string(),
            selected: 0,
        };

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 16))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Resume"));
        assert!(rendered.contains("Copy session ID"));
        assert!(!rendered.contains("Archive"));
        assert!(!rendered.contains("Delete"));
    }

    #[test]
    fn session_picker_phases_and_terminal_statuses_render_in_bounded_frames() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let phases = [
            (SessionPickerPhase::Browsing, "Current session"),
            (
                SessionPickerPhase::Actions {
                    session_id: "session-1".to_string(),
                    selected: 0,
                },
                "Session actions",
            ),
            (
                SessionPickerPhase::Renaming {
                    session_id: "session-1".to_string(),
                    value: "new title".to_string(),
                },
                "New title:",
            ),
            (
                SessionPickerPhase::ConfirmArchive {
                    session_id: "session-1".to_string(),
                    title: "Current session".to_string(),
                    selected: 0,
                },
                "Archive \"Current session\"?",
            ),
            (
                SessionPickerPhase::ConfirmDelete {
                    session_id: "session-1".to_string(),
                    title: "Current session".to_string(),
                    selected: 0,
                },
                "Permanently delete \"Current session\"?",
            ),
        ];

        for (width, height) in [(80, 24), (40, 12)] {
            for (phase, expected) in &phases {
                let mut state = test_state();
                state.status = AppStatus::SessionPicker;
                state.session_picker_sessions =
                    vec![session_summary("session-1", "Current session")];
                state.session_picker_phase = phase.clone();
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");

                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw picker phase");
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    rendered.contains(expected),
                    "missing {expected:?} for {phase:?} at {width}x{height}"
                );
                assert_eq!(
                    terminal.backend().buffer().content().len(),
                    usize::from(width) * usize::from(height)
                );
            }

            for (status, expected) in [
                ("completed", "(completed)"),
                ("indeterminate", "(state unknown)"),
            ] {
                let mut state = test_state();
                state.messages.push(ChatMessage::ToolCall {
                    id: status.to_string(),
                    name: "deploy".to_string(),
                    target: None,
                    status: status.to_string(),
                    output: Some("terminal result".to_string()),
                    diff: None,
                    kind: Some("result".to_string()),
                    expanded: false,
                });
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");

                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw terminal status");
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    rendered.contains(expected),
                    "missing {expected:?} at {width}x{height}: {rendered}"
                );
            }
        }
    }

    #[test]
    fn workspace_relative_path_label_prefers_longest_matching_runtime_root() {
        let roots = vec!["/workspace".into(), "/workspace/project".into()];

        assert_eq!(
            workspace_relative_path_label(Path::new("/workspace/project"), &roots),
            ":workspace_roots"
        );
        assert_eq!(
            workspace_relative_path_label(Path::new("/workspace/project/docs"), &roots),
            ":workspace_roots/docs"
        );
        assert_eq!(
            workspace_relative_path_label(Path::new("/var/tmp/cache"), &roots),
            "/var/tmp/cache"
        );
    }

    #[test]
    fn waiting_approval_does_not_render_composer_under_dialog() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
            tool: "web_search".to_string(),
            target: Some("A股 2026年6月30日 尾盘资金走向".to_string()),
            preview: None,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Approval Required"));
        assert!(
            !rendered.contains("Input"),
            "approval modal should own the foreground without drawing the idle composer"
        );
    }

    #[test]
    fn completed_plan_renders_implementation_prompt_without_composer() {
        let mut state = test_state();
        state.approval_mode = ApprovalMode::Plan;
        state.pre_plan_approval_mode = Some(ApprovalMode::AutoEdit);
        state.plan_approval_dialog = Some(PlanApprovalDialog {
            plan: "# Plan\n1. Inspect\n2. Implement".to_string(),
            selected: 0,
        });
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Implement this plan?"));
        assert!(rendered.contains("Yes, implement this plan"));
        assert!(rendered.contains("Switch to auto-edit and start coding."));
        assert!(rendered.contains("No, stay in Plan mode"));
        assert!(!rendered.contains("Input"));
    }

    #[test]
    fn waiting_approval_renders_numeric_shortcuts_in_semantic_order() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
            tool: "edit".to_string(),
            target: Some("src/main.rs".to_string()),
            preview: None,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        let once = rendered.find("[1] allow this once").expect("once option");
        let exact = rendered
            .find("[2] always allow this exact call")
            .expect("exact-call option");
        let tool = rendered
            .find("[3] always allow \"edit\"")
            .expect("tool-wide option");
        let deny = rendered.find("[4] deny").expect("deny option");

        assert!(once < exact);
        assert!(exact < tool);
        assert!(tool < deny);
        assert!(rendered.contains("1/2/3/4"));
        assert!(rendered.contains("legacy y/A/a/n"));
    }

    #[test]
    fn waiting_permission_approval_renders_specific_risk_title() {
        let mut state = test_state();
        state.update(TuiEvent::PermissionApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Permission, "approval-1"),
            tool: "bash".to_string(),
            target: Some("curl https://api.orca.invalid".to_string()),
            preview: Some("bash attempted network access to api.orca.invalid".to_string()),
            permission_kind:
                orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Network Permission Required"));
        assert!(!rendered.contains("Approval Required"));
    }

    #[test]
    fn approval_dialog_keeps_actions_visible_with_long_content() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-long"),
            tool: "bash".to_string(),
            target: Some(format!("echo {}", "very-long-target ".repeat(30))),
            preview: Some(
                (0..20)
                    .map(|index| format!("preview {index}: {}", "x".repeat(120)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        });
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20))
            .expect("test backend");

        terminal
            .draw(|frame| render_approval_dialog(frame, &state, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains('…'));
        assert!(rendered.contains("[1] allow this once"));
        assert!(rendered.contains("[2] always allow this exact call"));
        assert!(rendered.contains("[3] always allow \"bash\""));
        assert!(rendered.contains("[4] deny"));
        assert!(rendered.contains("preview truncated"));
        assert!(rendered.contains("↑↓ select · Enter · 1/2/3/4"));
    }

    #[test]
    fn long_slash_and_mention_menus_keep_selection_visible() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.slash_menu = Some(SlashMenu {
            items: (0..20)
                .map(|index| SlashMenuItem {
                    command: format!("/command-{index:02}"),
                    description: format!("command {index}"),
                })
                .collect(),
            selected: 19,
            sub_menu: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20))
            .expect("test backend");
        terminal
            .draw(|frame| {
                render_slash_menu(frame, Rect::new(0, 18, 70, 1), &state, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("/command-08"));
        assert!(rendered.contains("▸ /command-19"));
        assert!(!rendered.contains("/command-00"));

        state.slash_menu = None;
        state.mention.candidates = (0..20)
            .map(|index| {
                orca_runtime::mentions::MentionCandidate::from_file_match(
                    &orca_file_search::SearchMatch {
                        root: std::path::PathBuf::from("/workspace"),
                        path: format!("file-{index:02}.rs"),
                        kind: orca_file_search::MatchKind::File,
                        score: 1,
                        indices: Vec::new(),
                    },
                )
            })
            .collect();
        state.mention.selected = 19;
        state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 18, 70, 1), &state, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("@file-08.rs"));
        assert!(rendered.contains("▸ @file-19.rs"));
        assert!(!rendered.contains("@file-00.rs"));
    }

    #[test]
    fn skill_picker_uses_compact_dollar_rows_and_interaction_hint() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.mention.sigil = Some(orca_runtime::mentions::MentionSigil::Dollar);
        state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
        state.mention.candidates = (0..25)
            .map(|index| orca_runtime::mentions::MentionCandidate {
                id: format!("skill:skill-{index:02}"),
                kind: orca_runtime::mentions::MentionKind::Skill,
                display: format!("skill-{index:02}"),
                description: format!("Skill description {index}"),
                score: 1,
                indices: Vec::new(),
                target: orca_runtime::mentions::MentionTarget::Skill {
                    id: format!("skill-{index:02}"),
                    path: std::path::PathBuf::from(format!("/skills/skill-{index:02}/SKILL.md")),
                },
            })
            .collect();
        state.mention.selected = 12;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 12))
            .expect("test backend");

        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 10, 70, 1), &state, &theme);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Skills"));
        assert!(rendered.contains("▸ $skill-12"));
        assert!(rendered.contains("Skill description 12"));
        assert!(rendered.contains("13/25"));
        assert!(rendered.contains("PgUp/PgDn page"));
        assert!(!rendered.contains("$skill-00"));
        assert!(!rendered.contains("[skill]"));
    }

    #[test]
    fn compact_popup_hit_testing_matches_constrained_render_geometry() {
        let mut state = test_state();
        state.frame_area = Some(Rect::new(0, 0, 40, 16));
        state.input_area = Some(Rect::new(0, 12, 40, 3));
        state.slash_menu = Some(SlashMenu {
            items: (0..12)
                .map(|index| SlashMenuItem {
                    command: format!("/command-{index:02}"),
                    description: format!("command {index}"),
                })
                .collect(),
            selected: 11,
            sub_menu: None,
        });

        assert_eq!(slash_menu_hit_index(&state, 5, 10), Some(11));
        assert_eq!(slash_menu_hit_index(&state, 5, 12), None);

        state.slash_menu = None;
        state.mention.candidates = (0..12)
            .map(|index| {
                orca_runtime::mentions::MentionCandidate::from_file_match(
                    &orca_file_search::SearchMatch {
                        root: std::path::PathBuf::from("/workspace"),
                        path: format!("file-{index:02}.rs"),
                        kind: orca_file_search::MatchKind::File,
                        score: 1,
                        indices: Vec::new(),
                    },
                )
            })
            .collect();
        state.mention.selected = 11;
        state.mention.phase = Some(SearchPhase::Scanning);

        assert_eq!(mention_menu_hit_index(&state, 5, 9), Some(11));
        assert_eq!(mention_menu_hit_index(&state, 5, 10), None);
        assert_eq!(mention_menu_hit_index(&state, 5, 12), None);
    }

    #[test]
    fn popup_geometry_stays_in_frame_and_omits_unrenderable_status() {
        let frame = Rect::new(5, 7, 10, 2);
        let input = Rect::new(0, 9, 40, 3);

        let geometry = popup_geometry(frame, input, 0, 0, true).expect("border-only popup");

        assert_eq!(geometry.area, frame);
        assert!(!geometry.show_status);
        assert_eq!(geometry.start, geometry.end);
        assert_eq!(
            popup_geometry(frame, Rect::new(0, 7, 40, 3), 12, 11, false),
            None
        );
    }

    #[test]
    fn mention_popup_reports_every_streaming_phase() {
        assert_eq!(
            mention_status_text(&SearchPhase::Searching, 0, true),
            Some(("Searching files…".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Scanning, 42, false),
            Some(("Scanning… 42 paths".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Refreshing, 42, false),
            Some(("Refreshing…".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Complete, 42, true),
            Some(("No matches".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(
                &SearchPhase::Incomplete {
                    message: "walk failed".to_string(),
                },
                42,
                false,
            ),
            Some(("Search incomplete".to_string(), Color::Red))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Stopping, 42, false),
            Some(("Stopping search…".to_string(), Color::DarkGray))
        );
        assert_eq!(mention_status_text(&SearchPhase::Complete, 42, false), None);
    }

    #[test]
    fn mention_popup_highlights_unicode_character_indices() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.mention.candidates = vec![orca_runtime::mentions::MentionCandidate::from_file_match(
            &orca_file_search::SearchMatch {
                root: std::path::PathBuf::from("/workspace"),
                path: "src/你好.rs".to_string(),
                kind: orca_file_search::MatchKind::File,
                score: 1,
                indices: vec![4],
            },
        )];
        state.mention.phase = Some(SearchPhase::Complete);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");

        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 5, 20, 1), &state, &theme);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let highlighted = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "你")
            .unwrap();
        assert_eq!(highlighted.style().fg, Some(theme.warning));
    }

    #[test]
    fn mention_popup_renders_in_a_narrow_terminal() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.mention.candidates = vec![orca_runtime::mentions::MentionCandidate::from_file_match(
            &orca_file_search::SearchMatch {
                root: std::path::PathBuf::from("/workspace"),
                path: "src/a-very-long-file-name.rs".to_string(),
                kind: orca_file_search::MatchKind::File,
                score: 1,
                indices: Vec::new(),
            },
        )];
        state.mention.phase = Some(SearchPhase::Scanning);
        state.mention.progress.scanned_paths = 10;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 5))
            .expect("test backend");

        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 4, 12, 1), &state, &theme);
            })
            .expect("draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Mentions"));
    }

    #[test]
    fn live_pane_honours_scroll_offset_when_content_overflows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let body = (0..50)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        // Auto-scroll on: the pane pins to the bottom and shows the last lines.
        let mut auto = test_state();
        auto.messages.push(ChatMessage::Assistant(body.clone()));
        auto.auto_scroll = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut auto, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("L49"), "auto-scroll should show the tail");
        assert!(
            !rendered.contains("L0 "),
            "auto-scroll should not show the very first line"
        );

        // Scrolled to the top: the pane shows the earliest lines instead of the tail.
        let mut scrolled = test_state();
        scrolled.messages.push(ChatMessage::Assistant(body));
        scrolled.auto_scroll = false;
        scrolled.scroll_offset = 0;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut scrolled, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(
            rendered.contains("L0"),
            "scroll-to-top should show the first line"
        );
        assert!(
            !rendered.contains("L49"),
            "scroll-to-top should not show the tail"
        );
    }

    #[test]
    fn live_pane_auto_scrolls_cjk_content_to_the_tail() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let body = (0..24)
            .map(|i| format!("第{i}行中文内容，用来测试首问长答案是否能正确顶到底部"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut state = test_state();
        state.messages.push(ChatMessage::Assistant(body));
        state.auto_scroll = true;

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(24, 8))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut state, &theme);
            })
            .expect("draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(
            rendered.contains("第23行"),
            "auto-scroll should pin the tail of long CJK content"
        );
    }

    #[test]
    fn completed_turn_auto_scrolls_markdown_table_tail_above_composer() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let diff = (0..96)
            .map(|_| {
                "+ .hero .meta-row { margin-top: 28px; display: flex; justify-content: center; gap: 32px; flex-wrap: wrap; font-size: 14px; opacity: 0.8; }"
            })
            .collect::<Vec<_>>()
            .join("\n");
        let answer =
            r#"报告已生成，保存在 `tavily-research-report.html`。下面是这份报告覆盖的核心内容概要：

📋 报告结构 (10 大章节)

| 章节 | 要点 |
| --- | --- |
| 一、公司概览 | 2024 年成立于以色列，CEO Rotem Weiss，定位 "AI Agent 的 Google" |
| 二、发展历程 | 成立 → 2025 年 17x 增长 → $25M Series A → 2026.02 被 Nebius $2.75 亿收购 |
| 三、核心产品与技术 | Search/Extract/Crawl/Research/MCP 五大 API，GAIA Benchmark SOTA |
| 四、定价模型 | Free (1K/月) → Developer ($20) → Pro ($150) → Enterprise 定制 |
| 五、竞争格局 | 与 Exa、Brave、Serper、Perplexity 的 8 维度横向对比 |
| 六、Nebius 收购分析 | $275M-$400M 交易，战略意义：补全 AI 云平台搜索能力 |
| 七、应用场景 | 编码助手/RAG/市场调研/新闻监控/学术文献 六大场景 |
| 八、关键洞察 | 成功原因 + 风险挑战 + 未来趋势判断 |
| 九、开发者资源 | SDK、MCP、LangChain、文档等速查链接 |
| 十、总结 | Agentic Search 正在成为 AI 基础设施标配 |

你可以直接在浏览器中打开 `tavily-research-report.html`
查看完整的可视化报告，支持响应式布局，手机和桌面均可阅读。"#
                .to_string();

        let mut textarea = TextArea::default();
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Input "),
        );
        for (width, height) in [
            (90, 24),
            (120, 32),
            (150, 42),
            (180, 52),
            (180, 63),
            (200, 70),
        ] {
            let mut state = test_state();
            state.status = AppStatus::Idle;
            state.auto_scroll = true;
            state.messages.push(ChatMessage::ToolCall {
                id: "tool-1".to_string(),
                name: "edit".to_string(),
                target: Some("site/styles.css".to_string()),
                status: "completed".to_string(),
                output: None,
                diff: Some(diff.clone()),
                kind: None,
                expanded: false,
            });
            state.messages.push(ChatMessage::Reasoning(
                "The HTML report has been created. Let me verify it and provide a summary to the user."
                    .to_string(),
            ));
            state.messages.push(ChatMessage::Assistant(answer.clone()));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("test backend");

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            let rendered = format!("{:?}", terminal.backend().buffer());

            assert!(
                rendered.contains("支持响应式布局"),
                "completed answer tail should be visible immediately at {width}x{height}, not only after the next prompt"
            );
            assert!(
                rendered.contains("Input"),
                "composer should remain pinned below the transcript at {width}x{height}"
            );
        }
    }

    #[test]
    fn context_cell_is_hidden_until_a_budget_is_known() {
        let state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        // limit_tokens == 0 means no turn has reported a budget yet.
        assert_eq!(context_cell(&state, &theme).content.as_ref(), "");
    }

    #[test]
    fn context_cell_starts_at_full_remaining_capacity() {
        let mut state = test_state();
        state.context_limit_tokens = 1_000_000;
        state.context_used_tokens = 0;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let cell = context_cell(&state, &theme);

        assert_eq!(cell.content.as_ref(), "  ·  context 100%");
        assert_eq!(cell.style.fg, Some(theme.success));
    }

    #[test]
    fn context_cell_shows_remaining_percentage() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let cell = context_cell(&state, &theme);
        // 25% of the window used means 75% remains.
        assert_eq!(cell.content.as_ref(), "  ·  context 75%");
        assert_eq!(cell.style.fg, Some(theme.success));
    }

    #[test]
    fn context_cell_clamps_used_at_full_window() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 1200; // over-full estimate clamps to 0% remaining
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let cell = context_cell(&state, &theme);
        assert_eq!(cell.content.as_ref(), "  ·  context 0%");
        assert_eq!(cell.style.fg, Some(theme.error));
    }

    #[test]
    fn context_cell_warns_then_errors_as_remaining_context_shrinks() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let mut warn = test_state();
        warn.context_limit_tokens = 1000;
        warn.context_used_tokens = 700; // 30% remains
        assert_eq!(context_cell(&warn, &theme).style.fg, Some(theme.warning));

        let mut danger = test_state();
        danger.context_limit_tokens = 1000;
        danger.context_used_tokens = 900; // 10% remains
        assert_eq!(context_cell(&danger, &theme).style.fg, Some(theme.error));
    }

    #[test]
    fn approval_modes_use_distinct_semantic_colors() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        assert_eq!(
            approval_mode_color(ApprovalMode::Suggest, &theme),
            theme.border
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::AutoEdit, &theme),
            theme.approval
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::FullAuto, &theme),
            theme.error
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::Plan, &theme),
            theme.plan_mode
        );

        let colors = [theme.border, theme.approval, theme.error, theme.plan_mode];
        for (index, color) in colors.iter().enumerate() {
            assert!(!colors[..index].contains(color));
        }
    }

    #[test]
    fn status_line_renders_each_approval_mode_in_its_semantic_color() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for mode in [
            ApprovalMode::Suggest,
            ApprovalMode::AutoEdit,
            ApprovalMode::FullAuto,
            ApprovalMode::Plan,
        ] {
            let mut state = test_state();
            state.approval_mode = mode;
            let width = 180u16;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1))
                .expect("test backend");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_status(frame, area, &state, &theme);
                })
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let row = (0..width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>();
            let marker = format!("  ·  {}", mode.as_str());
            let marker_start = row.find(&marker).expect("mode should be visible");
            let value_x = (marker_start + "  ·  ".len()) as u16;
            assert_eq!(
                buffer[(value_x, 0)].fg,
                approval_mode_color(mode, &theme),
                "wrong status color for {}",
                mode.as_str()
            );
        }
    }

    #[test]
    fn workspace_status_spans_keep_full_then_compact_cwd_with_git() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.cwd = "~/Documents/GitHub/blade-deepseek".to_string();
        state.workspace_git = Some(GitIdentity::Branch("feature/footer".to_string()));

        assert_eq!(
            workspace_status_spans(&state, &theme, 80)
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>(),
            "  ·  ~/Documents/GitHub/blade-deepseek  ·  git:feature/footer"
        );
        assert_eq!(
            workspace_status_spans(&state, &theme, 46)
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>(),
            "  ·  ~/…/blade-deepseek  ·  git:feature/footer"
        );
    }

    #[test]
    fn workspace_status_spans_drop_git_before_cwd_and_bound_unicode() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.cwd = "~/项目/👍🏽-workspace".to_string();
        state.workspace_git = Some(GitIdentity::Branch(
            "feature/a-branch-too-wide-for-the-cell".to_string(),
        ));

        let text = workspace_status_spans(&state, &theme, 18)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(text.starts_with("  ·  "));
        assert!(!text.contains("git:"));
        assert!(UnicodeWidthStr::width(text.as_str()) <= 18);
        assert_eq!(text.contains('👍'), text.contains('🏽'));
    }

    #[test]
    fn workspace_status_spans_label_detached_head() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.cwd = "/repo".to_string();
        state.workspace_git = Some(GitIdentity::Detached("5bbb60aa".to_string()));

        assert!(
            workspace_status_spans(&state, &theme, 40)
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
                .contains("git:@5bbb60aa")
        );
    }

    #[test]
    fn status_line_prioritizes_context_workspace_then_usage_and_shortcuts() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        state.usage.input_tokens = 8_000;
        state.usage.output_tokens = 664;
        state.usage.estimated_cost_usd = 0.003852;
        state.cwd = "~/Documents/GitHub/blade-deepseek".to_string();
        state.workspace_git = Some(GitIdentity::Branch("feature/footer".to_string()));
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let wide = status_line(&state, &theme, 180).to_string();
        assert!(wide.contains("context 75%"));
        assert!(wide.contains("~/Documents/GitHub/blade-deepseek"));
        assert!(wide.contains("git:feature/footer"));
        assert!(wide.contains("8.7k tokens"));
        assert!(wide.contains("F1 shortcuts"));

        let medium = status_line(&state, &theme, 92).to_string();
        assert!(medium.contains("context 75%"));
        assert!(medium.contains("blade-deepseek"), "{medium}");
        assert!(medium.contains("git:feature/footer"));
        assert!(!medium.contains("tokens"));
        assert!(!medium.contains("shortcuts"));

        let narrow = status_line(&state, &theme, 46).to_string();
        assert!(narrow.contains("auto-edit"));
        assert!(narrow.contains("context 75%"));
        assert!(!narrow.contains("git:"));
        assert!(!narrow.contains("blade-deepseek"));
    }

    #[test]
    fn provider_context_survives_same_revision_surface_sync_in_footer() {
        let mut state = test_state();
        let snapshot = SurfaceProjectionState {
            session_id: "goal-session".to_string(),
            title: "Goal session".to_string(),
            usage_revision: 1,
            usage: orca_core::cost_types::UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
        };
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            snapshot.clone(),
        )));
        state.update(TuiEvent::ContextUpdated {
            used_tokens: 393_527,
            limit_tokens: 1_000_000,
        });

        // A later operation batch carries the unchanged context snapshot.
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(snapshot)));

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        assert!(
            status_line(&state, &theme, 100)
                .to_string()
                .contains("context 60%")
        );
    }

    #[test]
    fn status_line_reserves_known_context_before_truncating_a_long_model() {
        let mut state = test_state();
        state.model_name = "a-very-long-model-name-that-would-fill-the-footer".to_string();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        state.cwd = "~/workspace".to_string();
        state.workspace_git = Some(GitIdentity::Branch("main".to_string()));
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let text = status_line(&state, &theme, 46).to_string();

        assert!(text.contains("auto-edit"));
        assert!(text.contains("context 75%"));
        assert!(!text.contains("~/workspace"));
        assert!(!text.contains("git:main"));
        assert!(UnicodeWidthStr::width(text.as_str()) <= 46);
    }

    #[test]
    fn status_line_is_pure_and_deterministic_for_captured_workspace_state() {
        let mut state = test_state();
        state.cwd = "~/repo".to_string();
        state.workspace_git = Some(GitIdentity::Branch("main".to_string()));
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let first = status_line(&state, &theme, 120);
        let second = status_line(&state, &theme, 120);
        assert_eq!(first, second);
    }

    #[test]
    fn responsive_status_line_keeps_mode_and_context_before_optional_metadata() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        state.usage.input_tokens = 8_000;
        state.usage.output_tokens = 664;
        state.usage.estimated_cost_usd = 0.003852;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let narrow = status_line(&state, &theme, 46).to_string();
        assert!(narrow.contains("auto-edit"));
        assert!(narrow.contains("context 75%"));
        assert!(!narrow.contains("tokens"));
        assert!(!narrow.contains("shortcuts"));

        let wide = status_line(&state, &theme, 180).to_string();
        // Token counts humanize (8664 → 8.7k) and sub-cent costs keep 4 decimals.
        assert!(wide.contains("8.7k tokens"));
        assert!(wide.contains("$0.0039"));
        // Drag-to-copy is native now; the old shift+drag hint is gone.
        assert!(!wide.contains("shift+drag"));
        assert!(wide.contains("shortcuts"));
    }

    #[test]
    fn status_line_hides_usage_until_tokens_accumulate() {
        let state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let text = status_line(&state, &theme, 180).to_string();
        assert!(!text.contains("tokens"));
        assert!(!text.contains('$'));
        assert!(text.contains("shortcuts"));
    }

    #[test]
    fn token_and_cost_formatting_scale_with_magnitude() {
        assert_eq!(format_token_count(950), "950");
        assert_eq!(format_token_count(8_664), "8.7k");
        assert_eq!(format_token_count(1_250_000), "1.2M");
        assert_eq!(format_cost(0.003852), "$0.0039");
        assert_eq!(format_cost(1.25), "$1.25");
    }

    #[test]
    fn jump_pill_appears_when_scrolled_up_and_leaves_with_follow() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        for index in 0..80 {
            state
                .messages
                .push(ChatMessage::System(format!("line {index}")));
        }
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        let mut draw = |state: &mut AppState| {
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Following the tail: no pill.
        let following = draw(&mut state);
        assert!(!following.contains("Jump to bottom"));
        assert_eq!(state.jump_to_bottom_area, None);

        // Scrolled up: the pill appears and registers its click target.
        state.scroll_up(10);
        let scrolled = draw(&mut state);
        assert!(scrolled.contains("Jump to bottom (click) ↓"));
        assert!(state.jump_to_bottom_area.is_some());

        // Messages landing while detached turn it into an unread counter.
        state.push_message(ChatMessage::System("late one".to_string()));
        let one = draw(&mut state);
        assert!(one.contains("1 new message (click) ↓"));
        state.push_message(ChatMessage::System("late two".to_string()));
        let two = draw(&mut state);
        assert!(two.contains("2 new messages (click) ↓"));

        // Back at the bottom: gone again, count cleared for the next detach.
        state.scroll_to_bottom();
        let back = draw(&mut state);
        assert!(!back.contains("Jump to bottom"));
        assert!(!back.contains("new message"));
        assert_eq!(state.jump_to_bottom_area, None);
        assert_eq!(state.unseen_messages, 0);

        // Messages arriving while FOLLOWING never count as unread.
        state.push_message(ChatMessage::System("seen".to_string()));
        assert_eq!(state.unseen_messages, 0);
        state.scroll_up(10);
        let detached_again = draw(&mut state);
        assert!(detached_again.contains("Jump to bottom (click) ↓"));
    }

    #[test]
    fn monochrome_jump_pill_reverses_completed_buffer_cells() {
        let theme = monochrome_theme();
        let textarea = TextArea::default();
        let mut state = test_state();
        for index in 0..80 {
            state
                .messages
                .push(ChatMessage::System(format!("line {index}")));
        }
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        state.scroll_up(10);
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");

        let pill = state.jump_to_bottom_area.expect("jump pill area");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(pill.x + 1, pill.y)].symbol(), "J");
        assert!(
            buffer[(pill.x + 1, pill.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn monochrome_selection_reverses_completed_transcript_buffer_cells() {
        let theme = monochrome_theme();
        let textarea = TextArea::default();
        let mut state = test_state();
        state.push_message(ChatMessage::System("abc".to_string()));
        let pos = crate::selection::SelectionPos { row: 0, col: 0 };
        state.selection = Some(crate::selection::TranscriptSelection::begin(pos));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn copy_notice_overlays_input_border_and_expires() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let vim = crate::vim::VimState::new(false);
        let textarea = crate::composer_textarea::make_textarea(&vim, &theme);
        let mut state = test_state();
        let staged_at = std::time::Instant::now();
        state.stage_clipboard_copy("hello".to_string(), staged_at);
        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("hello"));

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        let mut draw = |state: &mut AppState| {
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Fresh: the notice overlays the input box's top border, on the right.
        let fresh = draw(&mut state);
        assert!(fresh.contains("copied 5 chars to clipboard"));

        // Status line never carries the notice anymore.
        assert!(
            !status_line(&state, &theme, 120)
                .to_string()
                .contains("copied")
        );

        // Expired: gone again.
        state.copy_notice = state.copy_notice.take().map(|mut notice| {
            notice.at = staged_at
                .checked_sub(crate::types::AppState::COPY_NOTICE_TTL)
                .expect("test instant");
            notice
        });
        let expired = draw(&mut state);
        assert!(!expired.contains("copied"));

        // Oversized for OSC 52: the notice admits only the local clipboard
        // saw it instead of overclaiming a remote copy.
        state.stage_clipboard_copy(
            "x".repeat(crate::clipboard::OSC52_MAX_TEXT_BYTES + 1),
            staged_at,
        );
        let degraded = draw(&mut state);
        assert!(degraded.contains("(local clipboard only)"));
    }

    #[test]
    fn welcome_screen_text_is_selectable_and_copyable() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        assert!(state.messages.is_empty());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let area = state.transcript_area.expect("transcript area recorded");

        let mut scratch = TextArea::default();
        let now = std::time::Instant::now();
        let event_at = |kind, column, row| {
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                area.x,
                area.y,
            ),
            &mut state,
            &mut scratch,
            now,
        );
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                area.x + area.width - 1,
                area.y + 6,
            ),
            &mut state,
            &mut scratch,
            now,
        );
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                area.x + area.width - 1,
                area.y + 6,
            ),
            &mut state,
            &mut scratch,
            now,
        );

        let copied = state
            .pending_clipboard_copy
            .as_deref()
            .expect("welcome text should be copyable");
        assert!(!copied.trim().is_empty());
    }

    #[test]
    fn long_plan_steps_and_tool_targets_stay_on_single_rows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.current_plan = Some((
            None,
            vec![
                PlanItem {
                    step: "inspect the complete workspace topology and every package boundary"
                        .repeat(3),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    step: "run verification".to_string(),
                    status: PlanStatus::Pending,
                },
            ],
        ));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 4))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_panel(frame, area, &state, &theme);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let first_step = (0..40).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
        let second_step = (0..40).map(|x| buffer[(x, 2)].symbol()).collect::<String>();
        assert!(first_step.contains('…'));
        assert!(second_step.contains("run verification"));

        let tool = ChatMessage::ToolCall {
            id: "tool-long".to_string(),
            name: "bash".to_string(),
            target: Some("cargo test --workspace --all-features ".repeat(20)),
            status: "completed".to_string(),
            output: None,
            diff: None,
            kind: Some("success".to_string()),
            expanded: false,
        };
        let rendered = build_lines_for_messages(&[tool], &theme, 40, 0, false);
        assert_eq!(rendered[0].width(), 40);
        assert!(rendered[0].to_string().contains('…'));
        assert!(rendered[0].to_string().ends_with("(completed)"));
    }

    #[test]
    fn running_activity_line_shows_elapsed_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.running_started_at = Some(Instant::now() - Duration::from_secs(65));

        let (text, color) = activity_line(&state, &theme).expect("running shows an activity line");
        assert_eq!(text, "● running 1m 05s");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn active_goal_activity_line_adds_persisted_and_live_elapsed_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

        let (text, color) = activity_line(&state, &theme).expect("running shows an activity line");

        assert_eq!(text, "● running 13m 10s");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn active_goal_activity_line_never_decreases_across_continuations() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));
        let first = activity_line(&state, &theme).unwrap().0;

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state.update(TuiEvent::GoalStatus(Some(goal_with_elapsed(
            ThreadGoalStatus::Active,
            13 * 60 + 20,
        ))));
        state.update(TuiEvent::TurnStarted {
            turn: 2,
            task: None,
        });
        state.running_started_at = Some(Instant::now() - Duration::from_secs(5));
        let second = activity_line(&state, &theme).unwrap().0;

        assert_eq!(first, "● running 13m 10s");
        assert_eq!(second, "● running 13m 25s");
    }

    #[test]
    fn inactive_goal_does_not_change_the_current_turn_timer() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for status in [
            ThreadGoalStatus::Paused,
            ThreadGoalStatus::Blocked,
            ThreadGoalStatus::UsageLimited,
            ThreadGoalStatus::BudgetLimited,
            ThreadGoalStatus::Complete,
        ] {
            let mut state = test_state();
            state.status = AppStatus::Running;
            state.current_goal = Some(goal_with_elapsed(status, 13 * 60));
            state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

            assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
        }
    }

    #[test]
    fn active_goal_activity_line_clamps_negative_persisted_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, -20));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

        assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
    }

    #[test]
    fn compacting_activity_line_shows_context_status() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Compacting;

        let (text, color) =
            activity_line(&state, &theme).expect("compacting shows an activity line");

        assert_eq!(text, "● Compacting context...");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn idle_has_no_activity_line() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Idle;

        assert!(
            activity_line(&state, &theme).is_none(),
            "idle sessions must not render an activity line above the composer"
        );
    }

    #[test]
    fn idle_foreground_keeps_active_background_tasks_visible() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut task = workflow_task_for_agent_dashboard(
            "audit",
            "agent-1",
            orca_core::workflow_types::WorkflowAgentStatus::Running,
        );
        task.status = TaskStatus::Running;
        state.enter_running();
        state.update(crate::types::TuiEvent::WorkflowTasksUpdated { tasks: vec![task] });
        state.update(crate::types::TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let (text, color) =
            activity_line(&state, &theme).expect("active background task remains visible");

        assert_eq!(state.status, AppStatus::Idle);
        assert_eq!(text, "● 1 background task running");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn idle_foreground_prioritizes_background_tasks_needing_attention() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut running = workflow_task_for_agent_dashboard(
            "audit",
            "agent-1",
            orca_core::workflow_types::WorkflowAgentStatus::Running,
        );
        running.status = TaskStatus::Running;
        let mut approval = workflow_task_for_agent_dashboard(
            "deploy",
            "agent-2",
            orca_core::workflow_types::WorkflowAgentStatus::Running,
        );
        approval.status = TaskStatus::ApprovalRequired;
        state.status = AppStatus::Idle;
        state.workflow_panel.tasks = vec![running, approval];

        let (text, color) = activity_line(&state, &theme).expect("task attention remains visible");

        assert_eq!(text, "● 1 background task running · 1 needs approval");
        assert_eq!(color, theme.approval);
    }

    #[test]
    fn workflow_progress_label_summarizes_agents_and_phases() {
        let task = BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "Audit".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(3),
            workflow_progress: Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 5,
                running_agents: 2,
                completed_agents: 2,
                failed_agents: 1,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        assert_eq!(
            workflow_progress_label(&task),
            "agents 2/5, running 2, failed 1, phases 1/3"
        );
    }

    #[test]
    fn workflows_panel_renders_async_subagent_tasks() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-subagent".to_string(),
            task_type: TaskType::Subagent,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "inspect auth".to_string(),
            command: None,
            agent_type: Some("general".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some(
                "/repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js".to_string(),
            ),
            workflow_launch_input: Some(orca_core::workflow_types::WorkflowInput {
                name: Some("audit".to_string()),
                args: Some(serde_json::json!({ "target": "src" })),
                ..Default::default()
            }),
            workflow_final_summary: Some("completed with fallback review".to_string()),
            workflow_failure_count: 1,
            usage: Some(orca_core::cost_types::UsageTotals {
                input_tokens: 120,
                output_tokens: 30,
                cache_tokens: 10,
                estimated_cost_usd: 0.0000252,
            }),
            subagent_current_activity: Some("bash: cargo test".to_string()),
            subagent_turn: Some(2),
            last_activity_at_ms: Some(1_500),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 16))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("inspect auth"));
        assert!(rendered.contains("subagent"));
        assert!(rendered.contains("turn 2"));
        assert!(rendered.contains("150 tok"));
        assert!(rendered.contains("bash: cargo test"));
    }

    #[test]
    fn workflows_panel_renders_selected_workflow_agent_rows() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-workflow".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Completed,
            is_backgrounded: false,
            description: "Audit".to_string(),
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: vec![orca_core::task_types::WorkflowPhaseTaskSummary {
                name: "scan".to_string(),
                status: orca_core::workflow_types::WorkflowRunStatus::Failed,
                agent_count: 1,
                error: Some("scan failed".to_string()),
                fallback: Some("value".to_string()),
            }],
            workflow_agents: vec![orca_core::task_types::WorkflowAgentTaskSummary {
                call_id: "agent-1".to_string(),
                call_path: "root:1".to_string(),
                team: Some("backend".to_string()),
                status: orca_core::workflow_types::WorkflowAgentStatus::Completed,
                attempt: 2,
                max_attempts: 2,
                previous_errors: vec!["first attempt failed".to_string()],
                error: None,
                transcript_path: Some("/tmp/agent-1.json".to_string()),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(3_500),
                usage: Some(orca_core::cost_types::UsageTotals {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                    estimated_cost_usd: 0.0000252,
                }),
            }],
            workflow_script_path: Some(
                "/repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js".to_string(),
            ),
            workflow_launch_input: Some(orca_core::workflow_types::WorkflowInput {
                name: Some("audit".to_string()),
                args: Some(serde_json::json!({ "target": "src" })),
                ..Default::default()
            }),
            workflow_final_summary: Some("completed with fallback review".to_string()),
            workflow_failure_count: 1,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(2_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("root:1"));
        assert!(rendered.contains("team backend"));
        assert!(rendered.contains("scan"));
        assert!(rendered.contains("fallback value"));
        assert!(rendered.contains("scan failed"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("attempt 2/2"));
        assert!(rendered.contains("retry errors 1"));
        assert!(rendered.contains("elapsed 2s"));
        assert!(rendered.contains("150 tok"));
        assert!(rendered.contains("$0.000025"));
        assert!(rendered.contains("full result"));
        assert!(rendered.contains("/tmp/agent-1.json"));
        assert!(rendered.contains("run workflow-run-1"));
        assert!(
            rendered
                .contains("script /repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js")
        );
        assert!(rendered.contains("launch name=audit args={\"target\":\"src\"}"));
        assert!(rendered.contains("failures 1"));
        assert!(rendered.contains("final completed with fallback review"));
    }

    #[test]
    fn agents_panel_renders_all_workflow_agent_rows() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Agents;
        state.workflow_panel.tasks = vec![
            workflow_task_for_agent_dashboard(
                "audit",
                "scan",
                orca_core::workflow_types::WorkflowAgentStatus::Running,
            ),
            workflow_task_for_agent_dashboard(
                "review",
                "review",
                orca_core::workflow_types::WorkflowAgentStatus::Completed,
            ),
        ];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 18))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Agents"));
        assert!(rendered.contains("audit"));
        assert!(rendered.contains("review"));
        assert!(rendered.contains("scan"));
        assert!(rendered.contains("team scan"));
        assert!(rendered.contains("team review"));
        assert!(rendered.contains("root:scan"));
        assert!(rendered.contains("root:review"));
        assert!(rendered.contains("running"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("150 tok"));
    }

    #[test]
    fn workflow_panel_labels_main_session_tasks() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: false,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        assert_eq!(task_type_label(&task), "session");
        assert_eq!(task_detail_label(&task), "elapsed 3s");
    }

    #[test]
    fn workflow_panel_labels_backgrounded_main_session_tasks() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        assert!(task_detail_label(&task).starts_with("backgrounded • elapsed "));
    }

    #[test]
    fn workflow_panel_labels_backgrounded_approval_tool() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("task_list".to_string()),
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        assert_eq!(
            task_detail_label(&task),
            "waiting on task_list • backgrounded • elapsed 3s"
        );
    }

    #[test]
    fn workflows_panel_renders_selected_task_error_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Failed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: Some("model timed out".to_string()),
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("error"));
        assert!(rendered.contains("model timed out"));
    }

    #[test]
    fn workflows_panel_renders_selected_task_multiline_error_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Failed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: Some("first failure\nsecond failure\nthird failure\nfourth failure".to_string()),
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 14))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("error"));
        assert!(rendered.contains("first failure"));
        assert!(rendered.contains("second failure"));
        assert!(rendered.contains("third failure"));
        assert!(!rendered.contains("fourth failure"));
    }

    #[test]
    fn workflow_metadata_row_count_counts_bounded_task_detail_rows() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: Some("line one\nline two\nline three\nline four".to_string()),
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        assert_eq!(workflow_metadata_row_count(&task), 3);
    }

    #[test]
    fn workflows_panel_renders_selected_task_result_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: Some("summary ready".to_string()),
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("result"));
        assert!(rendered.contains("summary ready"));
    }

    #[test]
    fn workflows_panel_renders_contextual_action_hints_for_selected_task() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("↑↓ select"));
        assert!(rendered.contains("s stop"));
        assert!(rendered.contains("Esc close"));

        state.workflow_panel.tasks[0].status = TaskStatus::Completed;
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw terminal task");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("↑↓ select"));
        assert!(!rendered.contains("s stop"));
        assert!(rendered.contains("Esc close"));
    }

    #[test]
    fn workflows_panel_renders_foreground_action_hint_for_backgrounded_main_session() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Long answer".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("f foreground"));
    }

    #[test]
    fn workflows_panel_renders_approval_action_hint_for_selected_background_approval() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-approval".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "Needs approval".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("bash".to_string()),
            pending_tool_call: Some(orca_core::task_types::PendingToolCallSummary {
                id: "mock-tool-1".to_string(),
                name: "bash".to_string(),
                action: orca_core::approval_types::ActionKind::Write,
                target: Some("cargo test".to_string()),
                arguments: "{}".to_string(),
            }),
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(1_000),
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Enter approve"));
        assert!(rendered.contains("s stop"));
    }

    fn workflow_task_for_agent_dashboard(
        workflow_name: &str,
        call_suffix: &str,
        status: orca_core::workflow_types::WorkflowAgentStatus,
    ) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: format!("task-{workflow_name}"),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: workflow_name.to_string(),
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(workflow_name.to_string()),
            workflow_run_id: Some(format!("run-{workflow_name}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: vec![orca_core::task_types::WorkflowAgentTaskSummary {
                call_id: format!("agent-{call_suffix}"),
                call_path: format!("root:{call_suffix}"),
                team: Some(call_suffix.to_string()),
                status,
                attempt: 1,
                max_attempts: 2,
                previous_errors: Vec::new(),
                error: None,
                transcript_path: None,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(4_000),
                usage: Some(orca_core::cost_types::UsageTotals {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                    estimated_cost_usd: 0.0000252,
                }),
            }],
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }
    }

    #[test]
    fn workflow_panel_labels_retry_and_truncated_output() {
        let mut task = workflow_task_for_agent_dashboard(
            "audit",
            "agent-1",
            orca_core::workflow_types::WorkflowAgentStatus::Running,
        );
        task.retry_count = 2;
        task.output_truncated = true;

        let detail = task_detail_label(&task);
        assert!(detail.contains("retried 2"));
        assert!(detail.contains("output truncated"));
    }

    #[test]
    fn composer_layout_counts_soft_wrapped_visual_lines() {
        let mut textarea = TextArea::from(vec!["alpha bravo charlie".to_string()]);
        textarea.set_block(Block::default().borders(Borders::ALL));
        let theme = Theme::named(ThemeName::Dark);

        let layout = composer_visual_layout(12, &textarea, &theme);
        assert_eq!(composer_input_height(12, &textarea, &layout), 5);
    }

    #[test]
    fn composer_cursor_layout_tracks_ascii_and_cjk_display_columns() {
        let mut textarea = TextArea::from(["ab界c"]);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 3));

        let layout = textarea_visual_layout(&textarea, 20);

        assert_eq!(layout.cursor_visual_row, 0);
        assert_eq!(layout.cursor_display_col, 4);
        assert_eq!(layout.lines[0].to_string(), "ab界c");
    }

    #[test]
    fn composer_cursor_layout_tracks_combining_and_emoji_widths() {
        let mut textarea = TextArea::from(["e\u{301}🙂x"]);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 3));

        let layout = textarea_visual_layout(&textarea, 20);

        assert_eq!(
            layout.cursor_display_col,
            UnicodeWidthStr::width("e\u{301}🙂")
        );
    }

    #[test]
    fn composer_grapheme_layout_tracks_extended_emoji_widths() {
        for grapheme in ["👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
            let text = format!("a{grapheme}b");
            let mut textarea = TextArea::from([text.as_str()]);
            textarea.move_cursor(tui_textarea::CursorMove::Jump(
                0,
                (1 + grapheme.chars().count()) as u16,
            ));

            let layout = textarea_visual_layout(&textarea, 20);

            assert_eq!(layout.cursor_visual_row, 0, "{grapheme:?}");
            assert_eq!(
                layout.cursor_display_col,
                1 + UnicodeWidthStr::width(grapheme),
                "{grapheme:?}"
            );
            assert_eq!(layout.lines[0].to_string(), text, "{grapheme:?}");
        }
    }

    #[test]
    fn composer_internal_grapheme_cursor_uses_rendered_lead_column() {
        for grapheme in ["e\u{301}", "👍🏽", "1️⃣"] {
            let mut textarea = TextArea::from([grapheme]);
            textarea.move_cursor(tui_textarea::CursorMove::Forward);

            let layout = textarea_visual_layout(&textarea, 20);

            assert_eq!(textarea.cursor(), (0, 1), "{grapheme:?}");
            assert_eq!(layout.cursor_visual_row, 0, "{grapheme:?}");
            assert_eq!(layout.cursor_display_col, 0, "{grapheme:?}");
        }
    }

    #[test]
    fn composer_internal_grapheme_keeps_intact_width_and_cursor_cell() {
        for (text, width, cursor_grapheme) in [("e\u{301}x", 2, "e\u{301}"), ("👍🏽x", 3, "👍🏽")]
        {
            let mut textarea = TextArea::from([text]);
            textarea.move_cursor(tui_textarea::CursorMove::Forward);

            let layout = textarea_visual_layout(&textarea, width);
            let area = Rect::new(0, 0, width as u16, 1);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(
                Paragraph::new(layout.lines.clone()),
                area,
                &mut buffer,
            );

            assert_eq!(layout.lines.len(), 1, "{text:?}");
            assert_eq!(layout.lines[0].to_string(), text, "{text:?}");
            assert_eq!(layout.lines[0].width(), width, "{text:?}");
            assert_eq!(layout.cursor_display_col, 0, "{text:?}");
            assert_eq!(buffer[(0, 0)].symbol(), cursor_grapheme, "{text:?}");
            assert!(
                buffer[(0, 0)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED),
                "{text:?}"
            );
        }
    }

    #[test]
    fn composer_internal_grapheme_exact_width_uses_rendered_lead_cell() {
        for (grapheme, width) in [("e\u{301}", 1), ("👍🏽", 2), ("1️⃣", 2)] {
            let mut textarea = TextArea::from([grapheme]);
            textarea.move_cursor(tui_textarea::CursorMove::Forward);

            let layout = textarea_visual_layout(&textarea, width);
            let inner = Rect::new(7, 9, width as u16, 1);

            assert_eq!(layout.lines.len(), 1, "{grapheme:?}");
            assert_eq!(layout.lines[0].to_string(), grapheme, "{grapheme:?}");
            assert_eq!(layout.lines[0].width(), width, "{grapheme:?}");
            assert_eq!(layout.lines[0].spans.len(), 1, "{grapheme:?}");
            assert_eq!(layout.lines[0].spans[0].content.as_ref(), grapheme);
            assert_eq!(layout.lines[0].spans[0].style, textarea.cursor_style());
            assert_eq!(layout.cursor_visual_row, 0, "{grapheme:?}");
            assert_eq!(layout.cursor_display_col, 0, "{grapheme:?}");
            assert_eq!(
                visible_textarea_cursor(&layout, inner),
                Some(Position::new(inner.x, inner.y)),
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn composer_click_on_internal_grapheme_cursor_cell_preserves_logical_index() {
        for grapheme in ["e\u{301}", "👍🏽", "1️⃣"] {
            let width = 4;
            let mut textarea = TextArea::from([grapheme]);
            textarea.move_cursor(tui_textarea::CursorMove::Forward);
            let layout = textarea_visual_layout(&textarea, width);
            let area = Rect::new(10, 5, width as u16, 1);

            assert_eq!(layout.cursor_display_col, 0, "{grapheme:?}");
            assert_eq!(
                composer_click_target(&textarea, area, area.x, area.y),
                Some((0, 1)),
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn composer_grapheme_wrap_keeps_extended_emoji_clusters_intact() {
        for grapheme in ["👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
            let text = grapheme.repeat(2);
            let mut textarea = TextArea::from([text.as_str()]);
            textarea.move_cursor(tui_textarea::CursorMove::Jump(
                0,
                grapheme.chars().count() as u16,
            ));

            let layout = textarea_visual_layout(&textarea, 2);

            assert_eq!(
                rendered_text(&layout.lines),
                [grapheme, grapheme],
                "{grapheme:?}"
            );
            assert_eq!(layout.cursor_visual_row, 1, "{grapheme:?}");
            assert_eq!(layout.cursor_display_col, 0, "{grapheme:?}");
        }
    }

    #[test]
    fn composer_grapheme_exact_width_end_uses_styled_synthetic_row() {
        for grapheme in ["👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
            let mut textarea = TextArea::from([grapheme]);
            textarea.move_cursor(tui_textarea::CursorMove::End);

            let layout = textarea_visual_layout(&textarea, UnicodeWidthStr::width(grapheme));

            assert_eq!(layout.lines.len(), 2, "{grapheme:?}");
            assert_eq!(layout.lines[0].to_string(), grapheme, "{grapheme:?}");
            assert_eq!(layout.cursor_visual_row, 1, "{grapheme:?}");
            assert_eq!(layout.cursor_display_col, 0, "{grapheme:?}");
            assert_eq!(layout.lines[1].to_string(), " ", "{grapheme:?}");
            assert_eq!(
                layout.lines[1].spans[0].style,
                textarea.cursor_style(),
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn composer_combining_only_grapheme_keeps_cursor_and_click_target() {
        let text = "\u{301}";
        let mut textarea = TextArea::from([text]);
        textarea.move_cursor(tui_textarea::CursorMove::End);

        let layout = textarea_visual_layout(&textarea, 4);

        assert_eq!(textarea.cursor(), (0, 1));
        assert_eq!(layout.lines[0].to_string(), format!(" {text}"));
        assert_eq!(layout.cursor_visual_row, 0);
        assert_eq!(layout.cursor_display_col, 0);
        assert_eq!(
            layout.lines[0].spans.last().unwrap().style,
            textarea.cursor_style()
        );
        assert_eq!(
            composer_click_target(&textarea, Rect::new(0, 0, 4, 1), 0, 0),
            Some((0, 1))
        );
    }

    #[test]
    fn composer_combining_only_grapheme_survives_buffer_render_with_cursor() {
        let text = "\u{301}";
        let mut textarea = TextArea::from([text]);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        let layout = textarea_visual_layout(&textarea, 4);
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        ratatui::widgets::Widget::render(Paragraph::new(layout.lines), area, &mut buffer);

        assert_eq!(buffer[(0, 0)].symbol(), format!(" {text}"));
        assert!(
            buffer[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn composer_leading_combining_mark_attaches_without_losing_logical_start() {
        let text = "\u{301}a";
        let mut textarea = TextArea::from([text]);

        let start_layout = textarea_visual_layout(&textarea, 4);
        textarea.move_cursor(tui_textarea::CursorMove::Forward);
        let after_mark_layout = textarea_visual_layout(&textarea, 4);

        assert_eq!(start_layout.lines[0].to_string(), text);
        assert_eq!(start_layout.cursor_display_col, 0);
        assert_eq!(after_mark_layout.cursor_display_col, 0);
        assert_eq!(
            composer_click_target(&textarea, Rect::new(0, 0, 4, 1), 0, 0),
            Some((0, 1))
        );
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(Paragraph::new(start_layout.lines), area, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "a");
    }

    #[test]
    fn composer_zero_width_controls_preserve_source_order() {
        for text in [
            "a\u{200f}b",
            "a\u{200e}b",
            "a\u{200b}b",
            "a\u{2060}b",
            "\u{301}a",
        ] {
            let textarea = TextArea::from([text]);

            let layout = textarea_visual_layout(&textarea, 10);

            assert_eq!(layout.lines[0].to_string(), text, "{text:?}");
        }
    }

    #[test]
    fn composer_layout_tokenizes_each_logical_line_once() {
        let textarea = TextArea::from(["x".repeat(50_000)]);
        reset_textarea_grapheme_tokenization_count();

        let layout = textarea_visual_layout(&textarea, 10);

        assert!(layout.lines.len() > 1_000);
        assert_eq!(textarea_grapheme_tokenization_count(), 1);
    }

    #[test]
    fn composer_frame_reuses_the_height_layout_for_rendering() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::from(["x".repeat(500)]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        reset_textarea_grapheme_tokenization_count();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        assert_eq!(textarea_grapheme_tokenization_count(), 1);
    }

    #[test]
    fn hardware_cursor_matches_idle_composer_software_cursor() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        textarea.insert_str("ab界");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(5, 7));
        assert!(
            terminal.backend().buffer()[(5, 7)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn hardware_cursor_matches_wrapped_cjk_composer_software_cursor() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        textarea.insert_str("界界界界");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(8, 10)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(3, 7));
        assert!(
            terminal.backend().buffer()[(3, 7)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn main_composer_internal_grapheme_cursor_uses_rendered_lead_cell() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        for grapheme in ["1️⃣", "👍🏽", "e\u{301}"] {
            let mut state = test_state();
            let mut textarea =
                crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
            textarea.insert_str(grapheme);
            textarea.move_cursor(tui_textarea::CursorMove::Head);
            textarea.move_cursor(tui_textarea::CursorMove::Forward);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();

            let cursor = Position::new(1, 7);
            assert_eq!(textarea.cursor(), (0, 1), "{grapheme:?}");
            assert_eq!(textarea.lines(), &[grapheme.to_string()], "{grapheme:?}");
            terminal.backend_mut().assert_cursor_position(cursor);
            let buffer = terminal.backend().buffer();
            assert!(
                buffer[cursor].modifier.contains(Modifier::REVERSED),
                "{grapheme:?}: {:?}",
                buffer[cursor]
            );
            assert!(
                buffer
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains(grapheme),
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn unrenderable_wide_cursor_grapheme_hides_hardware_cursor() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let visible = TextArea::from(["a"]);
        let unrenderable = TextArea::from(["界"]);
        let (backend, events) = RecordingBackend::new(1, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_textarea_surface(frame, frame.area(), &visible, None, None, &theme, true);
            })
            .unwrap();
        assert_eq!(
            take_cursor_events(&events),
            [CursorEvent::Show, CursorEvent::Move(Position::new(0, 0))]
        );

        terminal
            .draw(|frame| {
                render_textarea_surface(
                    frame,
                    frame.area(),
                    &unrenderable,
                    None,
                    None,
                    &theme,
                    true,
                );
            })
            .unwrap();

        let cursor_events = take_cursor_events(&events);
        assert_eq!(cursor_events, [CursorEvent::Hide]);
        assert!(
            terminal
                .backend()
                .inner
                .buffer()
                .content()
                .iter()
                .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn exact_width_synthetic_row_completed_draw_uses_next_row_first_cell() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea = TextArea::from(["界"]);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(2, 2)).unwrap();

        terminal
            .draw(|frame| {
                render_textarea_surface(frame, frame.area(), &textarea, None, None, &theme, true);
            })
            .unwrap();

        let cursor = Position::new(0, 1);
        terminal.backend_mut().assert_cursor_position(cursor);
        let cursor_cell = &terminal.backend().buffer()[cursor];
        assert_eq!(cursor_cell.symbol(), " ");
        assert!(cursor_cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn editable_conversation_states_expose_the_hardware_cursor() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);

        for status in [
            AppStatus::Idle,
            AppStatus::Running,
            AppStatus::Compacting,
            AppStatus::WaitingUserInput,
        ] {
            let mut state = test_state();
            state.status = status.clone();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();

            terminal
                .backend_mut()
                .assert_cursor_position(Position::new(1, 7));
            assert!(
                terminal.backend().buffer()[(1, 7)]
                    .modifier
                    .contains(Modifier::REVERSED),
                "{status:?}"
            );
        }
    }

    #[test]
    fn composer_popups_keep_the_hardware_cursor() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);

        for popup in ["slash", "mention"] {
            let mut state = test_state();
            match popup {
                "slash" => {
                    state.slash_menu = Some(SlashMenu {
                        items: vec![SlashMenuItem {
                            command: "/help".to_string(),
                            description: "show help".to_string(),
                        }],
                        selected: 0,
                        sub_menu: None,
                    });
                }
                "mention" => {
                    state.mention.phase = Some(SearchPhase::Scanning);
                }
                _ => unreachable!(),
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();

            terminal
                .backend_mut()
                .assert_cursor_position(Position::new(1, 7));
        }
    }

    #[test]
    fn compact_tall_slash_menu_keeps_cursor_on_reversed_composer_cell() {
        let mut state = test_state();
        state.slash_menu = Some(SlashMenu {
            items: (0..12)
                .map(|index| SlashMenuItem {
                    command: format!("/command-{index:02}"),
                    description: format!("command {index}"),
                })
                .collect(),
            selected: 11,
            sub_menu: None,
        });
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 16)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        let cursor = Position::new(1, 13);
        terminal.backend_mut().assert_cursor_position(cursor);
        let cursor_cell = &terminal.backend().buffer()[cursor];
        assert_eq!(cursor_cell.symbol(), " ");
        assert!(cursor_cell.modifier.contains(Modifier::REVERSED));
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("/command-02"));
        assert!(rendered.contains("▸ /command-11"));
        assert!(!rendered.contains("/command-01"));
    }

    #[test]
    fn compact_tall_mention_menu_keeps_cursor_on_reversed_composer_cell() {
        let mut state = test_state();
        state.mention.candidates = (0..12)
            .map(|index| {
                orca_runtime::mentions::MentionCandidate::from_file_match(
                    &orca_file_search::SearchMatch {
                        root: std::path::PathBuf::from("/workspace"),
                        path: format!("file-{index:02}.rs"),
                        kind: orca_file_search::MatchKind::File,
                        score: 1,
                        indices: Vec::new(),
                    },
                )
            })
            .collect();
        state.mention.selected = 11;
        state.mention.phase = Some(SearchPhase::Scanning);
        state.mention.progress.scanned_paths = 42;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 16)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        let cursor = Position::new(1, 13);
        terminal.backend_mut().assert_cursor_position(cursor);
        let cursor_cell = &terminal.backend().buffer()[cursor];
        assert_eq!(cursor_cell.symbol(), " ");
        assert!(cursor_cell.modifier.contains(Modifier::REVERSED));
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Mentions"));
        assert!(rendered.contains("@file-03.rs"));
        assert!(rendered.contains("▸ @file-11.rs"));
        assert!(!rendered.contains("@file-02.rs"));
        assert!(rendered.contains("Scanning… 42 paths"));
    }

    #[test]
    fn vim_modes_keep_the_hardware_cursor_on_the_software_cursor() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        for (mode, cursor_color) in [
            (crate::vim::VimMode::Insert, theme.border),
            (crate::vim::VimMode::Normal, theme.warning),
            (crate::vim::VimMode::Visual, theme.approval),
        ] {
            let mut state = test_state();
            let mut vim = crate::vim::VimState::new(true);
            vim.mode = mode;
            let textarea = crate::composer_textarea::make_textarea(&vim, &theme);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();

            terminal
                .backend_mut()
                .assert_cursor_position(Position::new(1, 7));
            let cursor_cell = &terminal.backend().buffer()[(1, 7)];
            let cursor_style = cursor_cell.style();
            assert_eq!(cursor_style.fg, Some(cursor_color), "{mode:?}");
            assert_eq!(cursor_style.bg, Some(Color::Reset), "{mode:?}");
            assert_eq!(cursor_style.underline_color, Some(Color::Reset), "{mode:?}");
            assert_eq!(cursor_style.add_modifier, Modifier::REVERSED, "{mode:?}");
            assert_eq!(cursor_style.sub_modifier, Modifier::empty(), "{mode:?}");
        }
    }

    fn assert_hidden_frame_does_not_move_composer_cursor(mut configure: impl FnMut(&mut AppState)) {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        let (backend, events) = RecordingBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut state = test_state();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let editable_events = take_cursor_events(&events);
        assert!(editable_events.contains(&CursorEvent::Show));
        assert!(
            editable_events
                .iter()
                .any(|event| matches!(event, CursorEvent::Move(_))),
            "{editable_events:?}"
        );

        configure(&mut state);
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let hidden_events = take_cursor_events(&events);

        assert!(
            hidden_events.contains(&CursorEvent::Hide),
            "{hidden_events:?}"
        );
        assert!(
            !hidden_events
                .iter()
                .any(|event| matches!(event, CursorEvent::Move(_))),
            "{hidden_events:?}"
        );
    }

    #[test]
    fn waiting_approval_frame_hides_the_hardware_cursor_without_moving_it() {
        assert_hidden_frame_does_not_move_composer_cursor(|state| {
            state.update(TuiEvent::ApprovalNeeded {
                key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
                tool: "web_search".to_string(),
                target: Some("query".to_string()),
                preview: None,
            });
        });
    }

    #[test]
    fn session_picker_frame_hides_the_hardware_cursor_without_moving_it() {
        assert_hidden_frame_does_not_move_composer_cursor(|state| {
            state.status = AppStatus::SessionPicker;
        });
    }

    #[test]
    fn shortcuts_frame_hides_the_hardware_cursor_without_moving_it() {
        assert_hidden_frame_does_not_move_composer_cursor(|state| {
            state.show_shortcuts = true;
        });
    }

    #[test]
    fn setup_cursor_uses_masked_api_key_cell() {
        let mut state = test_state();
        state.status = AppStatus::Setup;
        state.setup_step = 1;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea = crate::composer_textarea::make_setup_textarea(&theme);
        textarea.insert_str("密钥abc");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();

        let cursor = Position::new(12, 14);
        terminal.backend_mut().assert_cursor_position(cursor);
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("密钥abc"));
        assert!(!rendered.contains("密钥"));
        assert!(!rendered.contains('密'));
        assert!(!rendered.contains('钥'));
        assert!(!rendered.contains("abc"));
        assert!(rendered.contains("*****"));
        assert_eq!(buffer[cursor].symbol(), " ");
        assert!(buffer[cursor].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn setup_cursor_events_match_the_active_setup_step() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea = crate::composer_textarea::make_setup_textarea(&theme);
        textarea.insert_str("密钥abc");

        for setup_step in [0, 1, 2] {
            let mut state = test_state();
            state.status = AppStatus::Setup;
            state.setup_step = setup_step;
            let (backend, events) = RecordingBackend::new(70, 20);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();

            let cursor_events = take_cursor_events(&events);
            let moves = cursor_events
                .iter()
                .filter(|event| matches!(event, CursorEvent::Move(_)))
                .copied()
                .collect::<Vec<_>>();
            if setup_step == 1 {
                assert_eq!(
                    cursor_events
                        .iter()
                        .filter(|event| **event == CursorEvent::Show)
                        .count(),
                    1,
                    "{cursor_events:?}"
                );
                assert_eq!(
                    cursor_events
                        .iter()
                        .filter(|event| **event == CursorEvent::Hide)
                        .count(),
                    0,
                    "{cursor_events:?}"
                );
                assert_eq!(
                    moves,
                    [CursorEvent::Move(Position::new(12, 14))],
                    "{cursor_events:?}"
                );
                let rendered = terminal
                    .backend()
                    .inner
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(!rendered.contains("密钥abc"));
                assert!(!rendered.contains("密钥"));
                assert!(!rendered.contains('密'));
                assert!(!rendered.contains('钥'));
                assert!(!rendered.contains("abc"));
                assert!(rendered.contains("*****"));
            } else {
                assert_eq!(
                    cursor_events
                        .iter()
                        .filter(|event| **event == CursorEvent::Hide)
                        .count(),
                    1,
                    "{cursor_events:?}"
                );
                assert_eq!(
                    cursor_events
                        .iter()
                        .filter(|event| **event == CursorEvent::Show)
                        .count(),
                    0,
                    "{cursor_events:?}"
                );
                assert!(moves.is_empty(), "{cursor_events:?}");
            }
        }
    }

    #[test]
    fn composer_cursor_hides_and_restores_across_consecutive_frames() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea =
            crate::composer_textarea::make_textarea(&crate::vim::VimState::new(false), &theme);
        let (backend, events) = RecordingBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut state = test_state();

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let first_idle_events = take_cursor_events(&events);
        assert_eq!(
            first_idle_events
                .iter()
                .filter(|event| **event == CursorEvent::Show)
                .count(),
            1,
            "{first_idle_events:?}"
        );
        assert_eq!(
            first_idle_events
                .iter()
                .filter(|event| **event == CursorEvent::Hide)
                .count(),
            0,
            "{first_idle_events:?}"
        );
        assert_eq!(
            first_idle_events
                .iter()
                .filter(|event| matches!(event, CursorEvent::Move(_)))
                .copied()
                .collect::<Vec<_>>(),
            [CursorEvent::Move(Position::new(1, 7))],
            "{first_idle_events:?}"
        );

        state.status = AppStatus::WaitingApproval;
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let waiting_events = take_cursor_events(&events);
        assert_eq!(
            waiting_events
                .iter()
                .filter(|event| **event == CursorEvent::Hide)
                .count(),
            1,
            "{waiting_events:?}"
        );
        assert_eq!(
            waiting_events
                .iter()
                .filter(|event| **event == CursorEvent::Show)
                .count(),
            0,
            "{waiting_events:?}"
        );
        assert!(
            !waiting_events
                .iter()
                .any(|event| matches!(event, CursorEvent::Move(_))),
            "{waiting_events:?}"
        );

        state.status = AppStatus::Idle;
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let second_idle_events = take_cursor_events(&events);
        assert_eq!(
            second_idle_events
                .iter()
                .filter(|event| **event == CursorEvent::Show)
                .count(),
            1,
            "{second_idle_events:?}"
        );
        assert_eq!(
            second_idle_events
                .iter()
                .filter(|event| **event == CursorEvent::Hide)
                .count(),
            0,
            "{second_idle_events:?}"
        );
        assert_eq!(
            second_idle_events
                .iter()
                .filter(|event| matches!(event, CursorEvent::Move(_)))
                .copied()
                .collect::<Vec<_>>(),
            [CursorEvent::Move(Position::new(1, 7))],
            "{second_idle_events:?}"
        );
    }

    #[test]
    fn composer_combining_trailing_mark_keeps_cursor_and_click_target() {
        let text = "a \u{301}";
        let mut textarea = TextArea::from([text]);
        textarea.move_cursor(tui_textarea::CursorMove::End);

        let layout = textarea_visual_layout(&textarea, 10);

        assert_eq!(textarea.cursor(), (0, 3));
        assert_eq!(layout.lines[0].to_string(), format!("{text} "));
        assert_eq!(layout.cursor_visual_row, 0);
        assert_eq!(layout.cursor_display_col, UnicodeWidthStr::width(text));
        assert_eq!(
            layout.lines[0].spans.last().unwrap().style,
            textarea.cursor_style()
        );
        assert_eq!(
            composer_click_target(&textarea, Rect::new(0, 0, 10, 1), 2, 0),
            Some((0, 3))
        );
        let area = Rect::new(0, 0, 10, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(Paragraph::new(layout.lines), area, &mut buffer);
        assert_eq!(buffer[(1, 0)].symbol(), " \u{301}");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
    }

    #[test]
    fn empty_composer_cursor_starts_at_origin_before_placeholder() {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("hint");

        let layout = textarea_visual_layout(&textarea, 20);

        assert_eq!(layout.cursor_visual_row, 0);
        assert_eq!(layout.cursor_display_col, 0);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.lines[0].spans[0].content.as_ref(), " ");
        assert_eq!(layout.lines[0].spans[0].style, textarea.cursor_style());
        assert_eq!(layout.lines[0].to_string(), " hint");
    }

    #[test]
    fn composer_cursor_at_word_wrap_boundary_uses_next_visual_row() {
        let mut textarea = TextArea::from(["alpha bravo"]);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 6));

        let layout = textarea_visual_layout(&textarea, 6);

        assert_eq!(layout.cursor_visual_row, 1);
        assert_eq!(layout.cursor_display_col, 0);
    }

    #[test]
    fn exact_width_line_end_creates_a_synthetic_cursor_row() {
        let mut textarea = TextArea::from(["abcdef"]);
        textarea.move_cursor(tui_textarea::CursorMove::End);

        let layout = textarea_visual_layout(&textarea, 6);

        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.cursor_visual_row, 1);
        assert_eq!(layout.cursor_display_col, 0);
        assert_eq!(layout.lines[0].to_string(), "abcdef");
        assert_eq!(layout.lines[1].to_string(), " ");
        assert_eq!(layout.lines[1].spans[0].style, textarea.cursor_style());
    }

    #[test]
    fn zero_width_layout_does_not_create_a_synthetic_cursor_row() {
        let mut textarea = TextArea::from(["abcdef"]);
        textarea.move_cursor(tui_textarea::CursorMove::End);

        let layout = textarea_visual_layout(&textarea, 0);

        assert_eq!(layout.lines.len(), 1);
    }

    #[test]
    fn hard_wrapped_token_cursor_uses_display_width_within_chunk() {
        let mut textarea = TextArea::from(["界界界"]);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 2));

        let layout = textarea_visual_layout(&textarea, 4);

        assert_eq!(layout.cursor_visual_row, 1);
        assert_eq!(layout.cursor_display_col, 0);
    }

    #[test]
    fn visible_composer_cursor_includes_origin_border_and_scroll() {
        let mut textarea = TextArea::from(["one", "two", "three", "four"]);
        textarea.set_block(Block::default().borders(Borders::ALL));
        textarea.move_cursor(tui_textarea::CursorMove::Bottom);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        let area = Rect::new(10, 5, 12, 4);
        let inner = textarea.block().unwrap().inner(area);
        let layout = textarea_visual_layout(&textarea, inner.width as usize);

        assert_eq!(
            visible_textarea_cursor(&layout, inner),
            Some(ratatui::layout::Position::new(inner.x + 4, inner.y + 1))
        );
    }

    #[test]
    fn masked_setup_layout_uses_mask_width_and_never_renders_secret() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut textarea = crate::composer_textarea::make_setup_textarea(&theme);
        textarea.insert_str("密钥abc");

        let layout = textarea_visual_layout(&textarea, 20);
        let rendered = layout.lines.iter().map(Line::to_string).collect::<String>();

        assert!(!rendered.contains("密钥abc"));
        assert!(rendered.contains("*****"));
        assert_eq!(layout.cursor_display_col, 5);
    }

    #[test]
    fn masked_narrow_layout_wraps_by_mask_and_preserves_logical_selection() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let secret = "密钥abc";
        let mut textarea = crate::composer_textarea::make_setup_textarea(&theme);
        textarea.insert_str(secret);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 1));
        textarea.start_selection();
        textarea.move_cursor(tui_textarea::CursorMove::End);

        let layout = textarea_visual_layout(&textarea, 2);

        assert_eq!(rendered_text(&layout.lines), ["**", "**", "* "]);
        assert_eq!(layout.cursor_visual_row, 2);
        assert_eq!(layout.cursor_display_col, 1);
        assert_eq!(
            layout.lines[2].spans.last().unwrap().style,
            textarea.cursor_style()
        );
        let selection_style = Style::default().bg(Color::LightBlue);
        let selected_chars = layout
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style == selection_style)
            .map(|span| span.content.chars().filter(|ch| *ch == '*').count())
            .sum::<usize>();
        assert_eq!(selected_chars, 4);
        for span in layout.lines.iter().flat_map(|line| &line.spans) {
            assert!(
                span.content.chars().all(|ch| ch == '*' || ch == ' '),
                "secret fragment leaked in span {:?}",
                span.content
            );
        }
    }

    #[test]
    fn monochrome_composer_selection_reverses_completed_buffer_cells() {
        let theme = monochrome_theme();
        let mut textarea = TextArea::from(["abc"]);
        textarea.set_cursor_style(Style::default());
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 0));
        textarea.start_selection();
        textarea.move_cursor(tui_textarea::CursorMove::Jump(0, 2));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(6, 1)).expect("test backend");

        terminal
            .draw(|frame| {
                render_textarea_surface(frame, frame.area(), &textarea, None, None, &theme, false);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn composer_click_maps_masked_and_cjk_display_cells() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut masked = crate::composer_textarea::make_setup_textarea(&theme);
        masked.insert_str("密钥abc");
        let area = Rect::new(10, 5, 4, 5);
        let inner = masked.block().unwrap().inner(area);
        assert_eq!(
            composer_click_target(&masked, area, inner.x + 1, inner.y + 1),
            Some((0, 3))
        );

        let cjk = TextArea::from(["界a"]);
        let area = Rect::new(0, 0, 6, 1);
        assert_eq!(composer_click_target(&cjk, area, 1, 0), Some((0, 0)));
        assert_eq!(composer_click_target(&cjk, area, 2, 0), Some((0, 1)));
    }

    #[test]
    fn composer_click_maps_extended_grapheme_cells_and_wrapped_boundaries() {
        for grapheme in ["👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
            let text = format!("{grapheme}x");
            let textarea = TextArea::from([text.as_str()]);
            let area = Rect::new(0, 0, 6, 1);
            assert_eq!(
                composer_click_target(&textarea, area, 1, 0),
                Some((0, 0)),
                "{grapheme:?}"
            );
            assert_eq!(
                composer_click_target(&textarea, area, 2, 0),
                Some((0, grapheme.chars().count() as u16)),
                "{grapheme:?}"
            );

            let wrapped_text = grapheme.repeat(2);
            let wrapped = TextArea::from([wrapped_text.as_str()]);
            assert_eq!(
                composer_click_target(&wrapped, Rect::new(0, 0, 2, 2), 0, 1),
                Some((0, grapheme.chars().count() as u16)),
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn composer_click_maps_word_wrap_and_synthetic_cursor_rows() {
        let wrapped = TextArea::from(["alpha bravo"]);
        assert_eq!(
            composer_click_target(&wrapped, Rect::new(0, 0, 6, 2), 0, 1),
            Some((0, 6))
        );

        let mut exact = TextArea::from(["abcdef"]);
        exact.move_cursor(tui_textarea::CursorMove::End);
        assert_eq!(
            composer_click_target(&exact, Rect::new(0, 0, 6, 2), 0, 1),
            Some((0, 6))
        );
    }

    #[test]
    fn hardware_cursor_rejects_empty_and_non_left_aligned_surfaces() {
        let textarea = TextArea::default();
        let layout = textarea_visual_layout(&textarea, 10);
        assert_eq!(visible_textarea_cursor(&layout, Rect::ZERO), None);

        let mut centered = TextArea::default();
        centered.set_alignment(ratatui::layout::Alignment::Center);
        let layout = textarea_visual_layout(&centered, 10);
        assert_eq!(
            visible_textarea_cursor(&layout, Rect::new(3, 4, 10, 1)),
            None
        );

        let mut layout = textarea_visual_layout(&TextArea::from(["abc"]), 10);
        layout.cursor_display_col = 10;
        assert_eq!(
            visible_textarea_cursor(&layout, Rect::new(3, 4, 10, 1)),
            None
        );

        layout.cursor_display_col = 2;
        assert_eq!(
            visible_textarea_cursor(&layout, Rect::new(u16::MAX - 1, 4, 10, 1)),
            None
        );

        layout.cursor_visual_row = layout.lines.len();
        assert_eq!(
            visible_textarea_cursor(&layout, Rect::new(3, 4, 10, 1)),
            None
        );
    }

    #[test]
    fn hardware_cursor_includes_nonzero_origin_without_block() {
        let mut textarea = TextArea::from(["abc"]);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        let layout = textarea_visual_layout(&textarea, 10);

        assert_eq!(layout.lines[0].to_string(), "abc ");
        assert_eq!(
            visible_textarea_cursor(&layout, Rect::new(7, 9, 10, 1)),
            Some(ratatui::layout::Position::new(10, 9))
        );
    }

    #[test]
    fn composer_render_soft_wraps_long_pasted_lines() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::from(vec!["alpha bravo charlie".to_string()]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 8))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("bravo"));
        assert!(rendered.contains("charlie"));
    }

    #[test]
    fn composer_cursor_at_wrap_boundary_belongs_to_next_visual_line() {
        let mut textarea = TextArea::default();
        for ch in "alpha bravo".chars() {
            textarea.insert_char(ch);
        }
        for _ in 0.."bravo".chars().count() {
            textarea.move_cursor(tui_textarea::CursorMove::Back);
        }

        let layout = textarea_visual_layout(&textarea, 6);

        assert_eq!(layout.cursor_visual_row, 1);
    }

    /// Wrapped height the scroll math sees for `text` at `width` — the same
    /// `Paragraph::line_count` call `render_live_messages` uses.
    fn measured_rows(text: &str, width: u16) -> usize {
        Paragraph::new(Line::from(text))
            .wrap(Wrap { trim: false })
            .line_count(width)
    }

    #[test]
    fn line_count_matches_ratatui_word_wrap() {
        assert_eq!(measured_rows("alpha bravo charlie", 10), 3);
    }

    #[test]
    fn line_count_hard_wraps_long_tokens() {
        assert_eq!(measured_rows("abcdefghijkl", 5), 3);
    }

    #[test]
    fn line_count_keeps_hyphenated_tokens_whole() {
        // ratatui breaks only on whitespace, so "bb-cc-dd" is one 8-wide token that
        // wraps as a unit after "aa": "aa" / "bb-cc-" / "dd" = 3 rows. A measure that
        // also broke on '-' would pack tighter and undercount to 2, under-scrolling the
        // newest content out of view.
        assert_eq!(measured_rows("aa bb-cc-dd", 6), 3);
    }

    #[test]
    fn complete_assistant_line_advances_only_the_tail_message_revision() {
        let mut state = test_state();
        state.push_message(ChatMessage::User("prompt".to_string()));
        state.update(TuiEvent::MessageDelta("alpha bravo charlie\n".to_string()));
        let before = state.message_revisions.clone();

        state.update(TuiEvent::MessageDelta(" delta".to_string()));

        assert_eq!(state.message_revisions[0], before[0]);
        assert_eq!(state.message_revisions[1], before[1]);

        state.update(TuiEvent::MessageDelta("\n".to_string()));

        assert_eq!(state.message_revisions[0], before[0]);
        assert_ne!(state.message_revisions[1], before[1]);
    }

    #[test]
    fn streaming_newline_gate_hides_partial_source_until_completion_frame() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 16))
            .expect("test backend");

        state.update(TuiEvent::MessageDelta(
            "visible line\nhidden half".to_string(),
        ));
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("streaming draw");
        let streaming = format!("{:?}", terminal.backend().buffer());
        assert!(streaming.contains("visible line"));
        assert!(!streaming.contains("hidden half"));

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("completed draw");
        let completed = format!("{:?}", terminal.backend().buffer());
        assert!(completed.contains("visible line"));
        assert!(completed.contains("hidden half"));

        let lines = build_lines_for_messages(&state.messages, &theme, 80, 0, false);
        assert_eq!(
            lines
                .iter()
                .rev()
                .take_while(|line| line.to_string().is_empty())
                .count(),
            1
        );
    }

    #[test]
    fn streaming_table_holdback_reveals_the_whole_table_at_one_boundary() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 16))
            .expect("test backend");

        for delta in ["| Name | Value |\n", "|---|---|\n", "| A | 1 |\n"] {
            state.update(TuiEvent::MessageDelta(delta.to_string()));
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("held table draw");
            let held = format!("{:?}", terminal.backend().buffer());
            assert!(!held.contains("Name"));
            assert!(!held.contains("Value"));
            assert!(!held.contains(" A "));
        }

        state.update(TuiEvent::MessageDelta("\n".to_string()));
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("released table draw");
        let released = format!("{:?}", terminal.backend().buffer());
        assert!(released.contains("Name"));
        assert!(released.contains("Value"));
        assert!(released.contains("A"));

        let mut completed = test_state();
        completed.update(TuiEvent::MessageDelta(
            "| Name | Value |\n|---|---|\n| B | 2 |\n".to_string(),
        ));
        completed.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        terminal
            .draw(|frame| render(frame, &mut completed, &textarea, &theme))
            .expect("completed table draw");
        let completed_frame = format!("{:?}", terminal.backend().buffer());
        assert!(completed_frame.contains("Name"));
        assert!(completed_frame.contains("Value"));
        assert!(completed_frame.contains("B"));
    }

    #[test]
    fn streaming_auto_follow_tracks_checkpoints_without_showing_partial_tail() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 16))
            .expect("test backend");

        for index in 0..100 {
            state.update(TuiEvent::MessageDelta(format!("block {index:03}\n\n")));
        }
        state.update(TuiEvent::MessageDelta("HIDDEN_PARTIAL_TAIL".to_string()));
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("streaming checkpoints draw");
        let streaming = format!("{:?}", terminal.backend().buffer());
        assert!(streaming.contains("block 099"));
        assert!(!streaming.contains("HIDDEN_PARTIAL_TAIL"));
        assert!(state.auto_scroll);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("completed checkpoints draw");
        let completed = format!("{:?}", terminal.backend().buffer());
        assert!(completed.contains("HIDDEN_PARTIAL_TAIL"));
        assert!(state.auto_scroll);
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_after_large_diff() {
        let mut state = test_state();
        state.messages.push(ChatMessage::User(
            "生成一份长报告，并在最后输出固定尾部标记。".to_string(),
        ));
        let diff = (0..96)
            .map(|index| {
                format!(
                    "+     .summary-card-{index:02} {{ grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); margin-bottom: 30px; border-radius: 12px; }}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::ToolCall {
            id: "tool-write".to_string(),
            name: "write_file".to_string(),
            target: Some("stock_report_20260702.html".to_string()),
            status: "completed".to_string(),
            output: Some("wrote report".to_string()),
            diff: Some(diff),
            kind: Some("success".to_string()),
            expanded: false,
        });
        let mut answer = String::new();
        answer.push_str("HTML 报告已生成：`/tmp/stock_report_20260702.html`\n\n");
        answer.push_str("📊 7月2日早市速览\n");
        for index in 1..=32 {
            answer.push_str(&format!(
                "• 第 {index:02} 条：板块分化剧烈，资金偏好在高股息、防御资产与成长题材之间快速切换，需要关注成交量、波动率和风险偏好变化。\n"
            ));
        }
        answer.push_str("EXACT_TAIL_VISIBLE_20260702");
        state.messages.push(ChatMessage::Assistant(answer));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(
            rendered.contains("EXACT_TAIL_VISIBLE_20260702"),
            "completed assistant tail marker should be visible above the composer; rendered buffer:\n{rendered}"
        );
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_after_large_diff_and_markdown_table() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut failures = Vec::new();
        for width in [92, 120, 160, 210, 260] {
            for height in [24, 36, 48, 64, 72] {
                let mut state = completed_table_tail_state();
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");

                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw");
                let rendered = format!("{:?}", terminal.backend().buffer());
                if !rendered.contains("EXACT_TABLE_TAIL_VISIBLE_20260702") {
                    failures.push(format!("{width}x{height}"));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "completed assistant tail marker after a wide markdown table should be visible above the composer; missing at: {}",
            failures.join(", ")
        );
    }

    fn completed_table_tail_state() -> AppState {
        let mut state = test_state();
        state.messages.push(ChatMessage::User(
            "生成一份包含宽表格的市场报告，并在最后输出固定尾部标记。".to_string(),
        ));
        let diff = (0..96)
            .map(|index| {
                format!(
                    "+     .index-card-{index:02} {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); padding: 60px 40px 50px; border-radius: 14px; }}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::ToolCall {
            id: "tool-write".to_string(),
            name: "write_file".to_string(),
            target: Some("market_table_report_20260702.html".to_string()),
            status: "completed".to_string(),
            output: Some("wrote report".to_string()),
            diff: Some(diff),
            kind: Some("success".to_string()),
            expanded: false,
        });
        let mut answer = String::new();
        answer.push_str(
            "[thinking] The HTML report has been created. Let me provide a summary to the user.\n",
        );
        answer.push_str(
            "报告已生成，保存至 `/Users/bytedance/美股走势分析报告_2026年7月.html`。\n\n",
        );
        answer.push_str("📊 报告核心亮点\n\n");
        answer.push_str("| 章节 | 内容 |\n");
        answer.push_str("| --- | --- |\n");
        answer.push_str(
            "| 指数速览 | S&P 500 -0.62%、纳指 -1.21%、道指 -0.18%，盘中曾创新高但尾盘回落 |\n",
        );
        answer.push_str(
            "| Q2 回顾 | 纳指 Q2 狂飙 +21%，六年最佳；费半 +81%，历史最佳，但季末急跌预警 |\n",
        );
        answer.push_str("| 板块轮动 | 科技成长仍是主线，能源、金融和防御板块出现明显分化 |\n");
        answer.push_str("| 风险提示 | 估值扩张、流动性预期和财报窗口同时影响短线风险偏好 |\n");
        answer.push_str("| 后市展望 | 维持中性偏多，但需要观察成交量、波动率和资金流向的确认 |\n");
        answer.push_str("| 操作建议 | 仓位控制在 6-7 成，保留机动资金应对外围不确定性 |\n\n");
        for index in 1..=24 {
            answer.push_str(&format!(
                "• 第 {index:02} 条：表格之后的补充要点需要完整可见，不能停在表格开头或摘要中段。\n"
            ));
        }
        answer.push_str("EXACT_TABLE_TAIL_VISIBLE_20260702");
        state.messages.push(ChatMessage::Assistant(answer));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_with_mixed_width_cjk_runs() {
        // Long runs mixing 2-cell CJK and 1-cell ASCII are the worst case for wrap-height
        // accounting: when a row has 1 cell left and the next char is 2 cells wide,
        // ratatui wraps early and "wastes" the cell, so a cells/width estimate undercounts
        // rows. The undercount accumulates across paragraphs and used to push the newest
        // lines below the viewport. Each width's paragraphs were found by fuzzing the old
        // estimator against ratatui's real word wrapper (estimate < actual at that width).
        let cases: &[(u16, &[&str])] = &[
            (
                34,
                &[
                    "，d能dA栈首d全2芯业是、型环训（练力栈全3首b3/片闭b）芯a全d%b是型训）模d1闭2（I型、（能型模：/1首c模能训参fd芯，型）。栈闭a首闭力-3e环 a",
                    "I-（a片%首栈界）型型数参闭）界A栈d环力1ffI型参c训环数d能闭cI，模首界gb/片闭参22业f，）（e片2业能。闭是-数A。是d闭首数数%能是（2能1gga首是A是2模个（/c环f栈全栈片全-Ia能3环训 能是芯cb栈b环 是-力%d，f1A3a片%",
                    "是模1界：（模训a，数be环、cd全/b/这闭参c，能能e2。g，A业1力能环gIb全个能bb闭首1训芯：界）模Ic力界g芯首A全型数。e （-c模首A）环首-a）、）",
                ],
            ),
            (
                61,
                &[
                    "能模型2栈环型b能-AAA数（/c2（e-环/A栈A：、力栈。闭c环界个AA力b全这个d%bbI/力这闭数A数g、bb：1芯界Ie（-I环-（、：力，片a（（。g闭能：）A（是I练 3%练模界栈界能（%力能-%：/e片a%个界c练a2",
                    "，这数%首是（/1全业b是个型闭I片栈I、能（/环数环栈力片。cg数练（全是2业。训模芯闭1界）业是%业I数栈、个。个/-界参闭e环f个首，型：能、，-栈力栈全，是个 环f（练是力闭芯数栈1环芯，c模训业I",
                    "模/个Ie练A参/栈力全Ic。 型A）A界是c片fI练2全全a：能模（gb环模，芯：f片首，）/全1a型A环%这全片模：）：这3aA 、个训Ia参芯。e2这数：：c界fggg2训是3fa",
                ],
            ),
            (
                92,
                &[
                    "%这e全练、闭。环A，、-（参I这型g（能全界参环Ag3ba模g型，21Ac训界环c。g/练个2片片全1闭：能（片%闭a片g能）环业数eb闭%首栈）d3（I型I数a能片，参1界练1训（d栈e力-A 模数栈是c1数是个力3I%、ea",
                    "个2）芯片A闭d3业（闭2这数训1。数/界全c练型训能%A1）练型训2训首是芯%，数d界c闭是练栈b、片片/练芯训d2能数-数是f（3，模Ic -：数个这这、ecgI力：型是bd环b-，界，个23片环（，片片）。3ca3e参I",
                    "能全e是栈闭。型业模力数2模：d。这、2个32首、g片数闭芯界/练模界a-。：，，1是b栈闭模e训能，这。个个全力31能型界力能a是参个、3栈环参是（1（练dc、首（g片/个栈参闭训），I 1A闭c 芯首-业：，c",
                ],
            ),
        ];

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut failures = Vec::new();
        for &(width, paragraphs) in cases {
            for height in [24u16, 36, 48] {
                let mut state = test_state();
                state.messages.push(ChatMessage::User(
                    "输出多段中英混排长文本，并在最后输出固定尾部标记。".to_string(),
                ));
                let mut answer = String::new();
                for _ in 0..4 {
                    for paragraph in paragraphs {
                        answer.push_str(paragraph);
                        answer.push_str("\n\n");
                    }
                }
                answer.push_str("EXACT_CJK_TAIL_VISIBLE_20260702");
                state.messages.push(ChatMessage::Assistant(answer));
                state.update(TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                });

                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");
                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw");
                let rendered = format!("{:?}", terminal.backend().buffer());
                if !rendered.contains("EXACT_CJK_TAIL_VISIBLE_20260702") {
                    failures.push(format!("{width}x{height}"));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "auto-scrolled tail must stay visible for mixed-width CJK/ASCII runs; missing at: {}",
            failures.join(", ")
        );
    }

    #[test]
    fn streaming_deltas_keep_the_newest_line_visible_without_user_input() {
        // Mirrors the app loop: each TuiEvent is applied, then `scroll_to_bottom()` runs
        // while auto_scroll is on, then a frame is drawn. The newest streamed text must
        // be on screen after every frame — no manual scrolling.
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        for index in 0..120u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:混排AI模型栈能力闭环片全2芯业是、型环训（练力栈全3首b3/片闭b）尾标{index:03}\n\n"
            )));
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            let rendered = format!("{:?}", terminal.backend().buffer());
            assert!(
                rendered.contains(&format!("尾标{index:03}")),
                "delta {index} scrolled out of view; auto_scroll={} scroll_offset={} total={} visible={}",
                state.auto_scroll,
                state.scroll_offset,
                state.total_lines,
                state.visible_height,
            );
        }
    }

    #[test]
    fn stray_wheel_up_on_first_screen_does_not_break_streaming_follow() {
        // Reported regression: after the first screenful, new streamed content stopped
        // being followed. Trigger: a wheel-up (trackpad inertia counts) while the
        // transcript still fit on one screen disarmed auto-follow with no visual
        // feedback, so the pane silently stopped tracking once content overflowed.
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        for index in 0..60u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:混排AI模型栈能力闭环片全2芯业是、型环训（练力栈全3首b3/片闭b）尾标{index:03}\n\n"
            )));
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            // A stray wheel tick lands while everything still fits on the first screen.
            if index == 2 {
                state.scroll_up(3);
            }
            let rendered = format!("{:?}", terminal.backend().buffer());
            assert!(
                rendered.contains(&format!("尾标{index:03}")),
                "delta {index} scrolled out of view; auto_scroll={} scroll_offset={} total={} visible={}",
                state.auto_scroll,
                state.scroll_offset,
                state.total_lines,
                state.visible_height,
            );
        }
    }

    #[test]
    fn scrolling_back_to_bottom_mid_stream_re_arms_follow() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        let mut draw = |state: &mut AppState| {
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Stream well past one screen, then deliberately scroll up: follow disarms.
        for index in 0..40u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:内容片全芯业型环训练力栈全首片闭\n\n"
            )));
            draw(&mut state);
        }
        state.scroll_up(6);
        draw(&mut state);
        assert!(
            !state.auto_scroll,
            "deliberate scroll-up should disarm follow"
        );

        // Wheel back down until the bottom is reached: follow re-arms and new
        // deltas are tracked again without further input.
        while !state.auto_scroll {
            state.scroll_down(3);
            draw(&mut state);
        }
        state.update(TuiEvent::MessageDelta(
            "重新跟随后的新内容尾标RESUME\n\n".to_string(),
        ));
        let rendered = draw(&mut state);
        assert!(
            rendered.contains("尾标RESUME"),
            "after re-arming, new deltas must be visible again"
        );
    }

    #[test]
    fn ground_truth_ratatui_wraps_hyphenated_token_on_whitespace_only() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        // Render through the real widget at the same width the scroll math uses and count
        // rows that received any glyph. This pins `Paragraph::line_count` (an unstable
        // ratatui feature) to actual render behavior, so a semantic change in a ratatui
        // upgrade shows up here instead of as a mis-scrolled transcript.
        let area = Rect::new(0, 0, 6, 8);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(Line::from("aa bb-cc-dd"))
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);

        let used_rows = (0..area.height)
            .filter(|&y| (0..area.width).any(|x| !buffer[(x, y)].symbol().trim().is_empty()))
            .count();

        assert_eq!(used_rows, 3);
        assert_eq!(measured_rows("aa bb-cc-dd", 6), used_rows);
    }

    #[test]
    fn centered_rect_stays_inside_a_non_origin_inline_viewport() {
        use ratatui::layout::Rect;
        // Reproduces the approval-dialog panic: under the inline viewport the frame area is
        // anchored below the origin (the real crash had `Rect{x:0,y:31,width:90,height:24}`).
        // A popup centered relative to (0,0) lands above the buffer and panics in
        // `Buffer::index_of`. `centered_rect` must keep the popup fully inside `area`.
        let area = Rect::new(0, 31, 90, 24);
        let popup = centered_rect(area, 64, 12);
        assert!(
            popup.y >= area.y,
            "popup top {} above viewport {}",
            popup.y,
            area.y
        );
        assert!(
            popup.bottom() <= area.bottom(),
            "popup bottom {} past viewport {}",
            popup.bottom(),
            area.bottom()
        );
        assert!(popup.right() <= area.right());
        assert!(popup.x >= area.x);
    }

    #[test]
    fn centered_rect_clamps_oversized_popup_to_area() {
        use ratatui::layout::Rect;
        // A popup larger than the (small) inline viewport must shrink to fit, never overflow.
        let area = Rect::new(0, 10, 40, 6);
        let popup = centered_rect(area, 64, 20);
        assert_eq!(popup.width, area.width);
        assert_eq!(popup.height, area.height);
        assert!(popup.bottom() <= area.bottom());
        assert!(popup.right() <= area.right());
    }

    #[test]
    fn overflowing_transcript_keeps_input_and_status_pinned() {
        // Regression: a transcript taller than the screen must NOT squeeze the input box or
        // status line off-screen. The fixed chrome stays; the transcript yields. (Previously
        // the messages area used `Constraint::Min(5)`, which has higher solver priority than
        // the `Length` chrome and stole its rows when content overflowed.)
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.status = AppStatus::Idle;
        let body = (0..80)
            .map(|i| format!("数据行内容{i}测试"))
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::Assistant(body));
        state.auto_scroll = true;
        // Real composer carries a bordered "Input" block (3 rows tall), like make_textarea.
        let mut textarea = TextArea::default();
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Input "),
        );
        let h = 24u16;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, h))
            .expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let row_text =
            |y: u16| -> String { (0..50).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let has = |needle: &str| (0..h).any(|y| row_text(y).contains(needle));

        assert!(
            has("Input"),
            "input box must stay visible when the transcript overflows"
        );
        assert!(
            has("auto-edit"),
            "status line must stay visible when the transcript overflows"
        );
        // The composer (input) needs its full height; the messages area is everything above
        // the input + status, so visible_height must leave room for them.
        assert!(
            state.visible_height <= (h - 2) as usize,
            "messages area ({}) must not consume the input/status rows (term {h})",
            state.visible_height
        );
    }
}
