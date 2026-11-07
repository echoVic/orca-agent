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
    REWIND_MODE_CHOICES, REWIND_PREVIEW_FILES, REWIND_VISIBLE_CHECKPOINTS, rewind_list_window,
    rewind_mode_at, rewind_mode_description, rewind_mode_label, rewind_mode_unavailable_reason,
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
            show_preview,
            preview_scroll,
        } => (
            "Rewind · confirm",
            confirm_lines(
                rewind,
                *checkpoint,
                *mode,
                report,
                *selected,
                *show_preview,
                *preview_scroll,
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
        if rewind_mode_at(selected).is_some_and(|mode| mode.is_summarize()) {
            lines.push(Line::from(Span::styled(
                "Conversation",
                theme.accent_style().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "Files stay as they are; this action calls one summarization request.",
                theme.muted_style(),
            )));
            lines.push(Line::from(Span::styled(
                "The confirm page shows the summary plan after a dry-run.",
                theme.dim_style(),
            )));
        } else {
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

/// The confirm page: what the dry-run reported, with the warnings it raised,
/// the files it would skip, optionally the per-file unified diffs, then
/// Apply / Cancel. While the apply runs it says what the runtime is doing.
#[allow(clippy::too_many_arguments)]
fn confirm_lines(
    rewind: &RewindState,
    checkpoint: usize,
    mode: RewindMode,
    report: &RewindReport,
    selected: usize,
    show_preview: bool,
    preview_scroll: usize,
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
    if mode.is_summarize() {
        lines.push(Line::from(Span::styled(
            "Files unchanged by this action",
            Style::default().fg(theme.text),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "{} to restore · {} to delete · {} unchanged",
                files.changed, files.deleted, files.unchanged
            ),
            Style::default().fg(theme.text),
        )));
    }
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
    // The summarize request is the expensive part of those modes: say so
    // before the warnings, so a short window cannot cut it away.
    if mode.is_summarize() {
        lines.push(Line::from(Span::styled(
            "Summarize  one summarization request will run; it can take a while.",
            Style::default().fg(theme.warning),
        )));
        for warning in report
            .warnings
            .iter()
            .filter(|warning| warning.kind.contains("summar"))
        {
            let (text, style) = summarize_warning_text(warning, theme);
            lines.push(Line::from(Span::styled(
                truncate_to_display_width(&text, width),
                style,
            )));
        }
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
        for warning in report
            .warnings
            .iter()
            // A summarize request's warning was already shown above, with the
            // other summarize-mode details.
            .filter(|warning| !(mode.is_summarize() && warning.kind.contains("summar")))
        {
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

    let tail = 4 + usize::from(rewind.error.is_some()) + usize::from(rewind.pending);
    if show_preview {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Diff preview",
            theme.accent_style().add_modifier(Modifier::BOLD),
        )));
        let previews = diff_preview_lines(report, theme);
        if previews.is_empty() {
            lines.push(Line::from(Span::styled(
                "No file diffs: the dry-run returned none for this action.",
                theme.muted_style(),
            )));
        } else {
            let total = previews.len();
            let budget = rows
                .saturating_sub(lines.len() + tail + usize::from(total > 0))
                .max(1);
            let scroll = preview_scroll.min(total.saturating_sub(1));
            let end = (scroll + budget).min(total);
            lines.extend(previews.into_iter().skip(scroll).take(budget));
            if total > budget {
                lines.push(Line::from(Span::styled(
                    format!(
                        "… lines {}–{} of {total} · PgUp/PgDn scroll",
                        scroll + 1,
                        end
                    ),
                    theme.dim_style(),
                )));
            }
        }
    } else {
        // Keep the options and keys on screen in a short window: the body is
        // cut from the top of the middle, below the target line.
        while lines.len() + tail > rows && lines.len() > 3 {
            lines.remove(2);
        }
    }

    if rewind.pending {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if mode.is_summarize() {
                "Summarizing the conversation… (this can take a while)"
            } else {
                "Applying the rewind…"
            },
            Style::default().fg(theme.warning),
        )));
    }
    lines.push(Line::from(""));
    let apply_detail = if rewind.pending {
        "waiting for the runtime…"
    } else if mode.is_summarize() {
        "apply and summarize now"
    } else {
        "rewind now"
    };
    lines.push(crate::chrome::option_line(
        theme,
        selected == 0,
        "y",
        "Apply",
        6,
        apply_detail,
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
    let hints: &[(&str, &str)] = if rewind.pending {
        &[("Esc", "hide"), ("Ctrl+C", "close")]
    } else if mode.is_summarize() {
        &[
            ("y/Enter", "apply & summarize"),
            ("n/Esc", "back"),
            ("Tab", "diff"),
        ]
    } else {
        &[("y/Enter", "apply"), ("n/Esc", "back"), ("Tab", "diff")]
    };
    lines.push(crate::chrome::hint_line(theme, width, hints));
    lines
}

/// The warning a summarize mode's runtime adds about the request it will run
/// (the model, the cost), when it adds one.
fn summarize_warning_text(
    warning: &orca_core::rewind::RewindWarning,
    theme: &Theme,
) -> (String, Style) {
    let mut text = warning
        .detail
        .clone()
        .unwrap_or_else(|| format!("summarization: {}", warning.kind));
    if !warning.paths.is_empty() {
        text.push_str(": ");
        text.push_str(&warning.paths.join(", "));
    }
    (text, Style::default().fg(theme.warning))
}

/// The per-file unified diffs of a dry-run, each under its path with its line
/// stats; binary files and capped diffs are marked instead of rendered.
fn diff_preview_lines(report: &RewindReport, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for preview in report.previews.iter().take(REWIND_PREVIEW_FILES) {
        let mut header = vec![Span::styled(
            preview.path.clone(),
            theme.accent_style().add_modifier(Modifier::BOLD),
        )];
        header.push(Span::styled(
            format!("  +{} −{}", preview.insertions, preview.deletions),
            theme.muted_style(),
        ));
        if preview.binary {
            header.push(Span::styled("  binary", Style::default().fg(theme.warning)));
        }
        if preview.truncated {
            header.push(Span::styled(
                "  truncated",
                Style::default().fg(theme.warning),
            ));
        }
        lines.push(Line::from(header));
        if preview.binary {
            lines.push(Line::from(Span::styled(
                "  binary file: no text diff",
                theme.dim_style(),
            )));
            continue;
        }
        if preview.diff.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no diff text)",
                theme.dim_style(),
            )));
            continue;
        }
        lines.extend(crate::diff_highlight::render_unified_diff(
            &preview.diff,
            theme,
            None,
        ));
        if preview.truncated {
            lines.push(Line::from(Span::styled(
                "  … diff truncated",
                theme.dim_style(),
            )));
        }
    }
    if report.previews.len() > REWIND_PREVIEW_FILES {
        lines.push(Line::from(Span::styled(
            format!(
                "… and {} more file(s)",
                report.previews.len() - REWIND_PREVIEW_FILES
            ),
            theme.dim_style(),
        )));
    }
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

    fn preview(path: &str, diff: &str) -> orca_core::rewind::RewindFilePreview {
        orca_core::rewind::RewindFilePreview {
            path: path.to_string(),
            diff: diff.to_string(),
            truncated: false,
            binary: false,
            insertions: 1,
            deletions: 1,
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
            previews: Vec::new(),
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
        assert!(frame.contains("Cancel"), "{frame}");
        assert!(frame.contains("Files"), "{frame}");
        // Summarize modes are available: they are conversation-scoped, which
        // this checkpoint still has.
        assert!(
            frame
                .lines()
                .any(|line| line.contains("4  Summarize from here") && !line.contains("no file")),
            "{frame}"
        );
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
            show_preview: false,
            preview_scroll: 0,
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
    fn summarize_modes_are_available_and_carry_their_request_notice() {
        let view = checkpoint(0, true);
        assert_eq!(
            rewind_mode_unavailable_reason(&view, RewindMode::SummarizeFrom),
            None
        );
        assert_eq!(
            rewind_mode_unavailable_reason(&view, RewindMode::SummarizeUpTo),
            None
        );

        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(1, true)]);
        let report = orca_core::rewind::RewindReport {
            mode: "summarize_from".to_string(),
            conversation: Some(orca_core::rewind::ConversationRewindReport {
                kind: "summarize_from".to_string(),
                kept_messages: 4,
            }),
            warnings: vec![orca_core::rewind::RewindWarning {
                kind: "summarize_request".to_string(),
                paths: Vec::new(),
                detail: Some("model deepseek-flash · one request".to_string()),
            }],
            ..report("summarize_from")
        };
        rewind.phase = RewindPhase::Confirm {
            checkpoint: 0,
            mode: RewindMode::SummarizeFrom,
            report,
            selected: 0,
            show_preview: false,
            preview_scroll: 0,
        };
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 100, 34);
        assert!(frame.contains("Files unchanged by this action"), "{frame}");
        assert!(
            frame.contains("one summarization request will run"),
            "{frame}"
        );
        assert!(frame.contains("model deepseek-flash"), "{frame}");
        assert!(frame.contains("Conversation: summarize_from"), "{frame}");
        assert!(frame.contains("apply & summarize"), "{frame}");
    }

    #[test]
    fn the_confirm_page_renders_real_unified_diffs_on_tab() {
        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(1, true)]);
        let mut report = report("both");
        report.previews = vec![
            preview(
                "src/main.rs",
                "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-fn old() {}\n+fn new() {}\n",
            ),
            orca_core::rewind::RewindFilePreview {
                binary: true,
                truncated: true,
                ..preview("assets/logo.bin", "")
            },
        ];
        rewind.phase = RewindPhase::Confirm {
            checkpoint: 0,
            mode: RewindMode::Both,
            report,
            selected: 0,
            show_preview: true,
            preview_scroll: 0,
        };
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 120, 40);
        assert!(frame.contains("Diff preview"), "{frame}");
        assert!(frame.contains("src/main.rs"), "{frame}");
        assert!(frame.contains("fn old() {}"), "{frame}");
        assert!(frame.contains("fn new() {}"), "{frame}");
        assert!(frame.contains("assets/logo.bin"), "{frame}");
        assert!(frame.contains("binary file: no text diff"), "{frame}");
        // Both flags are marked on the file's header row.
        assert!(frame.contains("binary  truncated"), "{frame}");
        assert!(frame.contains("Tab diff"), "{frame}");
    }

    #[test]
    fn the_diff_preview_scrolls_and_says_where_it_is() {
        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(1, true)]);
        let mut report = report("both");
        let mut diff = String::from("--- a/big.rs\n+++ b/big.rs\n@@ -1,30 +1,30 @@\n");
        for line in 0..30 {
            diff.push_str(&format!("-old line {line}\n+new line {line}\n"));
        }
        report.previews = vec![preview("big.rs", &diff)];
        rewind.phase = RewindPhase::Confirm {
            checkpoint: 0,
            mode: RewindMode::Both,
            report,
            selected: 0,
            show_preview: true,
            preview_scroll: 20,
        };
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 120, 30);
        assert!(frame.contains("lines 21–"), "{frame}");
        assert!(frame.contains("PgUp/PgDn scroll"), "{frame}");
    }

    #[test]
    fn a_pending_summarize_says_what_the_runtime_is_doing() {
        let mut state = test_state();
        let mut rewind = RewindState::new(vec![checkpoint(1, true)]);
        rewind.phase = RewindPhase::Confirm {
            checkpoint: 0,
            mode: RewindMode::SummarizeUpTo,
            report: orca_core::rewind::RewindReport {
                mode: "summarize_up_to".to_string(),
                ..report("summarize_up_to")
            },
            selected: 0,
            show_preview: false,
            preview_scroll: 0,
        };
        rewind.pending = true;
        state.rewind = Some(rewind);
        let frame = frame_string(&mut state, 100, 34);
        assert!(frame.contains("Summarizing the conversation"), "{frame}");
        assert!(frame.contains("waiting for the runtime"), "{frame}");
    }
}
