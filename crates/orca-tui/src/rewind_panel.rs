//! The `/rewind` dialog: the conversation's checkpoints, the action menu of
//! a checkpoint, and the confirm page of that action's dry-run. The renderer
//! draws it over the conversation while `AppState::rewind` is set; its keys
//! are handled in `rewind_actions`.

use ratatui::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::display_text::truncate_to_display_width;
use crate::rewind_actions::{
    REWIND_MODE_CHOICES, REWIND_VISIBLE_CHECKPOINTS, rewind_list_window, rewind_mode_at,
    rewind_mode_description, rewind_mode_label, rewind_mode_unavailable_reason,
};
use crate::theme::Theme;
use crate::types::{AppState, RewindPhase, RewindState};
use orca_core::rewind::{RewindCheckpointView, RewindMode, RewindReport};

/// `/rewind`: the picker, whichever step it is on. Without a picker in
/// `state` it draws nothing.
pub(crate) fn render_rewind_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(rewind) = state.rewind.as_ref() else {
        return;
    };
    let area = frame.area();
    if area.width < 8 || area.height < 8 {
        return;
    }
    let width = dialog_width(area);
    let inner_width = usize::from(width.saturating_sub(4));
    // The borders, and the blank row plus the keys under the body.
    let body_rows = usize::from(area.height)
        .saturating_sub(4)
        .saturating_sub(2)
        .max(1);
    let (title, lines) = match &rewind.phase {
        RewindPhase::PickCheckpoint { selected, scroll } => (
            "Rewind",
            checkpoint_lines(rewind, *selected, *scroll, theme, inner_width, body_rows),
        ),
        RewindPhase::PickMode {
            checkpoint,
            selected,
            show_preview,
        } => (
            "Rewind · choose an action",
            mode_lines(
                rewind,
                *checkpoint,
                *selected,
                *show_preview,
                theme,
                inner_width,
                body_rows,
            ),
        ),
        RewindPhase::Confirm {
            checkpoint,
            mode,
            report,
            selected,
        } => (
            "Rewind · confirm",
            confirm_lines(
                rewind,
                *checkpoint,
                *mode,
                report,
                *selected,
                theme,
                inner_width,
                body_rows,
            ),
        ),
    };
    let popup = crate::chrome::dialog_rect(area, width, lines.len() as u16, area.height);
    frame.render_widget(Clear, popup);
    let block = crate::chrome::panel_block(theme, title, theme.border);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn dialog_width(area: ratatui::layout::Rect) -> u16 {
    let preferred = u16::try_from(u32::from(area.width) * 7 / 10).unwrap_or(u16::MAX);
    preferred.max(50).min(area.width.saturating_sub(4)).max(8)
}

/// One checkpoint per two rows: `#3  2h ago  fix the parser`, then what its
/// file snapshot holds.
fn checkpoint_lines(
    rewind: &RewindState,
    selected: usize,
    scroll: usize,
    theme: &Theme,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let len = rewind.checkpoints.len();
    if len == 0 {
        lines.push(Line::from(Span::styled(
            "No rewindable checkpoints.",
            theme.muted_style(),
        )));
    } else {
        let capacity = (rows / 2).max(1).min(REWIND_VISIBLE_CHECKPOINTS);
        let start = rewind_list_window(selected, scroll, len, capacity);
        let label_width = rewind
            .checkpoints
            .iter()
            .map(|view| UnicodeWidthStr::width(preview_text(view).as_str()))
            .max()
            .unwrap_or(0)
            .min(width.saturating_sub(14))
            .max(8);
        for (index, view) in rewind
            .checkpoints
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
        {
            let key = format!("#{}", RewindState::display_ordinal(view.ordinal));
            lines.push(crate::chrome::option_line(
                theme,
                index == selected,
                &key,
                &preview_text(view),
                label_width,
                &relative_time(view),
                width,
            ));
            lines.push(checkpoint_files_line(view, theme, width));
        }
    }
    lines.push(Line::from(""));
    lines.push(crate::chrome::hint_line(
        theme,
        width,
        &[
            ("↑↓", "move"),
            ("PgUp/PgDn", "page"),
            ("Enter", "choose"),
            ("Esc", "close"),
        ],
    ));
    lines
}

/// The second row of a checkpoint: `3 files changed +41 −7`,
/// `No code changes`, or why its files cannot be restored.
fn checkpoint_files_line(
    view: &RewindCheckpointView,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let (text, warning) = checkpoint_files_summary(view);
    let indent = "   ";
    let text = truncate_to_display_width(&text, width.saturating_sub(indent.len()));
    let style = if warning {
        Style::default().fg(theme.warning)
    } else {
        theme.dim_style()
    };
    Line::from(vec![
        Span::styled(indent.to_string(), theme.dim_style()),
        Span::styled(text, style),
    ])
}

fn checkpoint_files_summary(view: &RewindCheckpointView) -> (String, bool) {
    if !view.has_snapshot {
        if view.snapshot_evicted {
            return ("⚠ snapshot evicted (conversation only)".to_string(), true);
        }
        return ("⚠ no file snapshot (conversation only)".to_string(), true);
    }
    if view.files_changed == 0 {
        return ("No code changes".to_string(), false);
    }
    (
        format!(
            "{} files changed +{} −{}",
            view.files_changed, view.insertions, view.deletions
        ),
        false,
    )
}

/// The action menu of one checkpoint, with an optional preview of the files
/// a dry-run would report.
fn mode_lines(
    rewind: &RewindState,
    checkpoint: usize,
    selected: usize,
    show_preview: bool,
    theme: &Theme,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let view = rewind.checkpoints.get(checkpoint);
    let mut lines = Vec::new();
    lines.push(match view {
        Some(view) => Line::from(vec![
            Span::styled(
                format!("#{}  ", RewindState::display_ordinal(view.ordinal)),
                theme.accent_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_to_display_width(&preview_text(view), width.saturating_sub(5)),
                Style::default().fg(theme.text),
            ),
        ]),
        None => Line::from(Span::styled(
            "This checkpoint is no longer available.",
            theme.muted_style(),
        )),
    });
    lines.push(Line::from(""));
    let label_width = (1..=REWIND_MODE_CHOICES)
        .filter_map(|index| {
            rewind_mode_at(index - 1).map(|mode| UnicodeWidthStr::width(rewind_mode_label(mode)))
        })
        .chain(std::iter::once(UnicodeWidthStr::width("Cancel")))
        .max()
        .unwrap_or(0);
    for index in 0..REWIND_MODE_CHOICES {
        let key = (index + 1).to_string();
        match rewind_mode_at(index) {
            Some(mode) => match view.and_then(|view| rewind_mode_unavailable_reason(view, mode)) {
                Some(reason) => {
                    let label = rewind_mode_label(mode);
                    let padding = label_width.saturating_sub(UnicodeWidthStr::width(label));
                    let reason = truncate_to_display_width(&reason, width.saturating_sub(28));
                    lines.push(Line::from(vec![
                        Span::styled("   ", theme.dim_style()),
                        Span::styled(format!("{key}  "), theme.dim_style()),
                        Span::styled(format!("{label}{}", " ".repeat(padding)), theme.dim_style()),
                        Span::styled(format!("  {reason}"), theme.muted_style()),
                    ]));
                }
                None => lines.push(crate::chrome::option_line(
                    theme,
                    index == selected,
                    &key,
                    rewind_mode_label(mode),
                    label_width,
                    rewind_mode_description(mode),
                    width,
                )),
            },
            None => lines.push(crate::chrome::option_line(
                theme,
                index == selected,
                &key,
                "Cancel",
                label_width,
                "never mind",
                width,
            )),
        }
    }
    if show_preview {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Files",
            theme.accent_style().add_modifier(Modifier::BOLD),
        )));
        match view {
            Some(view) => {
                let (summary, warning) = checkpoint_files_summary(view);
                lines.push(Line::from(Span::styled(
                    summary,
                    if warning {
                        Style::default().fg(theme.warning)
                    } else {
                        theme.muted_style()
                    },
                )));
                for reason in &view.unavailable_reasons {
                    lines.push(Line::from(Span::styled(
                        format!("· {reason}"),
                        Style::default().fg(theme.warning),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "The confirm page shows the full plan after a dry-run.",
                    theme.dim_style(),
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "No preview is available.",
                theme.muted_style(),
            ))),
        }
    }
    if let Some(error) = &rewind.error {
        lines.push(Line::from(Span::styled(
            truncate_to_display_width(error, width),
            Style::default().fg(theme.error),
        )));
    }
    // Keep the keys on screen: the body may be taller than the window.
    while lines.len() + 2 > rows && lines.len() > REWIND_MODE_CHOICES + 1 {
        // Drop from the middle: the preview block first, then the header.
        let preview_start = lines.iter().position(|line| line.to_string() == "Files");
        match preview_start {
            Some(start) => {
                lines.truncate(start);
            }
            None => {
                lines.remove(1);
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(crate::chrome::hint_line(
        theme,
        width,
        &[
            ("↑↓", "choose"),
            ("1-6", "pick"),
            ("Enter", "confirm"),
            ("Tab", "files"),
            ("Esc", "back"),
        ],
    ));
    lines
}

/// The confirm page: what the dry-run reported, with the warnings it raised
/// and the files it would skip, then Apply / Cancel.
fn confirm_lines(
    rewind: &RewindState,
    checkpoint: usize,
    mode: RewindMode,
    report: &RewindReport,
    selected: usize,
    theme: &Theme,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let view = rewind.checkpoints.get(checkpoint);
    lines.push(match view {
        Some(view) => Line::from(vec![
            Span::styled(
                format!("#{}  ", RewindState::display_ordinal(view.ordinal)),
                theme.accent_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_to_display_width(&preview_text(view), width.saturating_sub(5)),
                Style::default().fg(theme.text),
            ),
        ]),
        None => Line::from(Span::styled(
            "This checkpoint is no longer available.",
            theme.muted_style(),
        )),
    });
    lines.push(Line::from(vec![
        Span::styled("Mode  ", theme.muted_style()),
        Span::styled(rewind_mode_label(mode), Style::default().fg(theme.text)),
    ]));
    lines.push(Line::from(""));
    // Files.
    let files = &report.files;
    lines.push(Line::from(Span::styled(
        "File plan",
        theme.accent_style().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "{} to restore · {} to delete · {} unchanged",
            files.changed, files.deleted, files.unchanged
        ),
        Style::default().fg(theme.text),
    )));
    if report.stats.insertions > 0 || report.stats.deletions > 0 || report.stats.omitted {
        let mut stats = format!("+{} −{}", report.stats.insertions, report.stats.deletions);
        if report.stats.omitted {
            stats.push_str(" (line counts omitted)");
        }
        lines.push(Line::from(Span::styled(stats, theme.muted_style())));
    }
    let mut skipped_slots = 3usize;
    for skip in &files.skipped {
        if skipped_slots == 0 {
            break;
        }
        skipped_slots -= 1;
        lines.push(Line::from(Span::styled(
            format!("Skip  {} ({})", skip.path, skip.reason),
            Style::default().fg(theme.warning),
        )));
    }
    // Warnings, which the user must see before applying.
    if !report.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            "Warnings",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )));
        let mut warning_slots = 6usize;
        for warning in &report.warnings {
            if warning_slots == 0 {
                break;
            }
            warning_slots -= 1;
            let mut text = warning_text(warning);
            if !warning.paths.is_empty() {
                text.push_str(": ");
                text.push_str(&warning.paths.join(", "));
            }
            if let Some(detail) = &warning.detail {
                text.push_str(" — ");
                text.push_str(detail);
            }
            lines.push(Line::from(Span::styled(
                truncate_to_display_width(&text, width),
                Style::default().fg(theme.warning),
            )));
        }
    }
    if let Some(conversation) = &report.conversation {
        lines.push(Line::from(Span::styled(
            format!(
                "Conversation: {} · keeps {} messages",
                conversation.kind, conversation.kept_messages
            ),
            theme.muted_style(),
        )));
    }
    // Keep the options and keys on screen in a short window: the body is
    // cut from the top of the middle, below the target line.
    let tail = 4; // blank, two options, keys
    let budget = rows.saturating_sub(3).max(tail);
    while lines.len() + tail > budget && lines.len() > 3 {
        lines.remove(2);
    }
    lines.push(Line::from(""));
    lines.push(crate::chrome::option_line(
        theme,
        selected == 0,
        "y",
        "Apply",
        6,
        "rewind now",
        width,
    ));
    lines.push(crate::chrome::option_line(
        theme,
        selected == 1,
        "n",
        "Cancel",
        6,
        "go back",
        width,
    ));
    if let Some(error) = &rewind.error {
        lines.push(Line::from(Span::styled(
            truncate_to_display_width(error, width),
            Style::default().fg(theme.error),
        )));
    }
    lines.push(crate::chrome::hint_line(
        theme,
        width,
        &[("y/Enter", "apply"), ("n/Esc", "back")],
    ));
    lines
}

fn warning_text(warning: &orca_core::rewind::RewindWarning) -> String {
    match warning.kind.as_str() {
        "untracked_change" => "files changed since the snapshot (overwritten)".to_string(),
        "root_mismatch" => "the snapshot is for a different workspace root".to_string(),
        "blob_missing" => "the snapshot is missing file content".to_string(),
        "snapshot_evicted" => "the snapshot was evicted by retention".to_string(),
        "pending_journal" => "an earlier rewind was interrupted".to_string(),
        other => format!("{other}"),
    }
}

/// The first line of a checkpoint's prompt, as the rows show it.
fn preview_text(view: &RewindCheckpointView) -> String {
    let preview = view
        .prompt_preview
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if preview.is_empty() {
        "(empty prompt)".to_string()
    } else {
        preview
    }
}

fn relative_time(view: &RewindCheckpointView) -> String {
    let Some(at) = view.recorded_at else {
        return String::new();
    };
    let now = chrono::Utc::now();
    if at > now {
        return "just now".to_string();
    }
    let seconds = (now - at).num_seconds();
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::frame_string;
    use crate::types::{RewindPhase, RewindState};
    use crossbeam_channel as mpsc;

    fn test_state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0".to_string(),
            "deepseek".to_string(),
            "/tmp".to_string(),
        )
    }

    fn checkpoint(ordinal: u32, files: bool) -> RewindCheckpointView {
        RewindCheckpointView {
            message_id: format!("item_{ordinal}"),
            ordinal,
            prompt_preview: format!("fix parser bug {ordinal}"),
            recorded_at: None,
            files_changed: if files { 3 } else { 0 },
            insertions: 41,
            deletions: 7,
            has_snapshot: files,
            snapshot_evicted: false,
            conversation_available: true,
            files_available: files,
            unavailable_reasons: Vec::new(),
        }
    }

    fn report(mode: &str) -> RewindReport {
        RewindReport {
            target_message_id: "item_1".to_string(),
            ordinal: 1,
            mode: mode.to_string(),
            dry_run: true,
            files: orca_core::rewind::RewindFilesReport {
                changed: 3,
                deleted: 1,
                unchanged: 5,
                skipped: vec![orca_core::rewind::RewindSkip {
                    path: "a.bin".to_string(),
                    reason: "blob missing".to_string(),
                }],
            },
            stats: orca_core::rewind::RewindStats {
                insertions: 41,
                deletions: 7,
                omitted: false,
            },
            warnings: vec![orca_core::rewind::RewindWarning {
                kind: "untracked_change".to_string(),
                paths: vec!["docs/x.md".to_string()],
                detail: None,
            }],
            conversation: Some(orca_core::rewind::ConversationRewindReport {
                kind: "truncate".to_string(),
                kept_messages: 12,
            }),
            rewind_id: None,
            op_id: None,
        }
    }

    #[test]
    fn the_checkpoint_list_renders_both_rows_per_checkpoint() {
        let mut state = test_state();
        state.rewind = Some(RewindState::new(vec![
            checkpoint(0, true),
            checkpoint(1, false),
        ]));
        let frame = frame_string(&mut state, 100, 30);
        assert!(frame.contains("#1"), "{frame}");
        assert!(frame.contains("#2"), "{frame}");
        assert!(frame.contains("fix parser bug 0"), "{frame}");
        assert!(frame.contains("3 files changed +41 −7"), "{frame}");
        assert!(
            frame.contains("⚠ no file snapshot (conversation only)"),
            "{frame}"
        );
        assert!(frame.contains("Enter choose"), "{frame}");
        assert!(frame.contains("Rewind"), "{frame}");
    }

    #[test]
    fn the_mode_menu_greys_out_unavailable_modes_with_a_reason() {
        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(0, false)]);
        rewind.phase = RewindPhase::PickMode {
            checkpoint: 0,
            selected: 0,
            show_preview: true,
        };
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 100, 30);
        assert!(frame.contains("Restore code and conversation"), "{frame}");
        assert!(frame.contains("no file snapshot"), "{frame}");
        assert!(frame.contains("not implemented yet"), "{frame}");
        assert!(frame.contains("Cancel"), "{frame}");
        assert!(frame.contains("Files"), "{frame}");
    }

    #[test]
    fn the_confirm_page_shows_the_plan_warnings_and_skipped_files() {
        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(1, true)]);
        rewind.phase = RewindPhase::Confirm {
            checkpoint: 0,
            mode: RewindMode::Both,
            report: report("both"),
            selected: 0,
        };
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 100, 34);
        assert!(
            frame.contains("3 to restore · 1 to delete · 5 unchanged"),
            "{frame}"
        );
        assert!(frame.contains("Skip  a.bin (blob missing)"), "{frame}");
        assert!(frame.contains("docs/x.md"), "{frame}");
        assert!(frame.contains("keeps 12 messages"), "{frame}");
        assert!(frame.contains("Apply"), "{frame}");
        assert!(frame.contains("y/Enter apply"), "{frame}");
    }

    #[test]
    fn no_dialog_is_drawn_without_a_rewind_state() {
        let mut state = test_state();
        let frame = frame_string(&mut state, 100, 30);
        assert!(!frame.contains("Rewind"), "{frame}");
    }

    #[test]
    fn a_narrow_window_still_fits_the_dialog() {
        let mut state = test_state();
        state.rewind = Some(RewindState::new(vec![checkpoint(0, true)]));
        let frame = frame_string(&mut state, 60, 20);
        assert!(frame.contains("Rewind"), "{frame}");
        assert!(frame.contains("#1"), "{frame}");
    }

    #[test]
    fn relative_time_reads_as_an_age() {
        let mut view = checkpoint(0, true);
        assert_eq!(relative_time(&view), "");
        view.recorded_at = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
        assert_eq!(relative_time(&view), "5m ago");
        view.recorded_at = Some(chrono::Utc::now() - chrono::Duration::hours(3));
        assert_eq!(relative_time(&view), "3h ago");
        view.recorded_at = Some(chrono::Utc::now() - chrono::Duration::days(2));
        assert_eq!(relative_time(&view), "2d ago");
    }

    #[test]
    fn a_summarize_mode_is_labelled_not_implemented() {
        let view = checkpoint(0, true);
        let reason = rewind_mode_unavailable_reason(&view, RewindMode::SummarizeFrom);
        assert_eq!(reason.as_deref(), Some("not implemented yet"));
    }
}
