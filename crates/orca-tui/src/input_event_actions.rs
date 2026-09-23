use std::time::Instant;

use crossbeam_channel as mpsc;
use crossterm::event::{Event, MouseButton, MouseEventKind};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::clipboard_image::{ImagePasteRequest, image_paths_from_paste};
use crate::composer_image_actions::begin_image_paste;
use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::{insert_composer_paste, insert_pasted_text};
use crate::selection::{SelectionGranularity, TranscriptSelection};
use crate::session_picker_actions::load_next_session_page;
use crate::terminal_presentation::TerminalPresentation;
use crate::types::{AppState, AppStatus, PanelMode, SessionPickerPhase};

#[derive(Debug, PartialEq)]
pub(crate) enum BatchedInputEvent {
    ScrollLines(i32),
    Event(Event),
}

/// Whether an input event is worth queueing at all.
///
/// crossterm's `EnableMouseCapture` turns on any-motion tracking (mode 1003),
/// so the terminal reports pointer movement with NO button held. Orca has no
/// hover UI, and the event-loop iteration marks the frame dirty for every
/// queued event — merely gliding the mouse across the window would redraw at
/// full frame rate. Drop motion events at intake instead.
pub(crate) fn should_queue_input_event(event: &Event) -> bool {
    !matches!(
        event,
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved
    )
}

pub(crate) fn consume_focus_event(event: &Event, presentation: &mut TerminalPresentation) -> bool {
    match event {
        Event::FocusGained => {
            presentation.set_focused(true);
            true
        }
        Event::FocusLost => {
            presentation.set_focused(false);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod focus_tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::consume_focus_event;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};

    fn presentation() -> TerminalPresentation {
        TerminalPresentation::new(
            true,
            TerminalPresentationProfile {
                osc9_supported: true,
                tmux_passthrough: false,
            },
        )
    }

    #[test]
    fn consume_focus_event_updates_only_presentation_focus() {
        let mut presentation = presentation();
        presentation.set_focused(false);

        assert!(consume_focus_event(&Event::FocusGained, &mut presentation));
        assert!(presentation.is_focused());
        assert!(consume_focus_event(&Event::FocusLost, &mut presentation));
        assert!(!presentation.is_focused());
        assert!(!consume_focus_event(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut presentation
        ));
        assert!(!presentation.is_focused());
    }
}

#[cfg(test)]
mod search_paste_tests {
    use crossterm::event::Event;
    use tui_textarea::TextArea;

    use super::handle_paste_event;
    use crate::composer_textarea::textarea_text;
    use crate::test_support::test_run_config;
    use crate::types::AppState;

    #[test]
    fn search_paste_updates_query_without_touching_composer() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.open_transcript_search();
        let config = test_run_config();
        let mut textarea = TextArea::from(["composer"]);

        assert!(handle_paste_event(
            &Event::Paste("one\r\ntwo".to_string()),
            &mut state,
            &config,
            &tx,
            &mut textarea,
        ));

        assert_eq!(state.transcript.search.query(), "one two");
        assert_eq!(textarea_text(&textarea), "composer");
        assert!(state.pending_pastes.is_empty());
    }
}

#[cfg(test)]
mod image_path_paste_tests {
    use crossterm::event::Event;
    use tui_textarea::TextArea;

    use super::handle_paste_event;
    use crate::clipboard_image::ImagePasteRequest;
    use crate::protocol::UserAction;
    use crate::test_support::test_run_config;
    use crate::types::AppState;

    #[test]
    fn pasted_image_path_routes_to_background_attachment_reader() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("screen shot.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            root.path().display().to_string(),
        );
        let mut textarea = TextArea::from(["caption"]);

        assert!(handle_paste_event(
            &Event::Paste(format!("'{}'", path.display())),
            &mut state,
            &test_run_config(),
            &tx,
            &mut textarea,
        ));

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::PasteImages {
                request: ImagePasteRequest::Paths(paths),
                ..
            }) if paths == vec![path]
        ));
        assert_eq!(textarea.lines(), &["caption".to_string()]);
    }
}

#[cfg(test)]
mod running_paste_tests {
    use crossterm::event::Event;
    use orca_core::config::ThemeName;
    use tui_textarea::TextArea;

    use super::handle_paste_event;
    use crate::composer_textarea::textarea_text;
    use crate::queued_input_actions::{enqueue_composer_follow_up, restore_latest_queued_message};
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::types::AppState;
    use crate::vim::VimState;

    #[test]
    #[ignore = "legacy local queue admission shim removed"]
    fn running_large_paste_queues_placeholder_and_restores_payload() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let config = test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();
        let pasted = "secret payload\n".repeat(100);

        assert!(handle_paste_event(
            &Event::Paste(pasted.clone()),
            &mut state,
            &config,
            &tx,
            &mut textarea,
        ));
        let placeholder = textarea_text(&textarea);
        assert!(placeholder.starts_with("[Pasted Content "));

        assert!(enqueue_composer_follow_up(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        state.set_status(crate::types::AppStatus::Idle);
        assert!(state.begin_next_queued_message().is_some());
        state.commit_queued_submission_admission();
        assert_eq!(state.input_history.last().unwrap(), pasted.trim());
        state.fail_queued_submission_dispatch("follow-up action queue is full".to_string());
        state.set_status(crate::types::AppStatus::Running);
        assert!(restore_latest_queued_message(&mut state, &tx,));
        assert_eq!(textarea_text(&textarea), placeholder);
        assert_eq!(state.pending_pastes.len(), 1);
        assert_eq!(state.pending_pastes[0].1, pasted);
    }
}

/// A terminal resize re-wraps every transcript line, so content positions
/// captured under the old width no longer describe the same text. Drop the
/// selection rather than let it highlight (and copy) unrelated rows.
pub(crate) fn handle_resize_event(ev: &Event, state: &mut AppState) -> bool {
    if !matches!(ev, Event::Resize(..)) {
        return false;
    }
    state.invalidate_selection();
    true
}

pub(crate) fn coalesce_input_events(
    events: impl IntoIterator<Item = Event>,
    wheel_step: i32,
) -> Vec<BatchedInputEvent> {
    let mut batched = Vec::new();
    let mut pending_scroll = 0i32;

    let flush_scroll = |batched: &mut Vec<BatchedInputEvent>, pending: &mut i32| {
        if *pending != 0 {
            batched.push(BatchedInputEvent::ScrollLines(*pending));
            *pending = 0;
        }
    };

    for event in events {
        match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                if pending_scroll > 0 {
                    flush_scroll(&mut batched, &mut pending_scroll);
                }
                pending_scroll = pending_scroll.saturating_sub(wheel_step);
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
                if pending_scroll < 0 {
                    flush_scroll(&mut batched, &mut pending_scroll);
                }
                pending_scroll = pending_scroll.saturating_add(wheel_step);
            }
            event => {
                flush_scroll(&mut batched, &mut pending_scroll);
                batched.push(BatchedInputEvent::Event(event));
            }
        }
    }
    flush_scroll(&mut batched, &mut pending_scroll);
    batched
}

pub(crate) fn handle_scroll_lines(state: &mut AppState, lines: i32, now: Instant) {
    let steps = ((lines.unsigned_abs() as usize) / 3).max(1);
    let upward = lines < 0;

    // The wheel drives whichever list currently has focus: the session
    // picker, an open popup menu, the workflows panel — or the transcript.
    if state.plan_approval_dialog.is_some() {
        if upward {
            state.scroll_up(steps);
        } else {
            state.scroll_down(steps);
        }
        return;
    }
    if state.status == AppStatus::SessionPicker {
        if state.session_picker_phase != SessionPickerPhase::Browsing {
            return;
        }
        if !upward
            && state.filtered_session_indices().last().copied()
                == Some(state.session_picker_selected)
        {
            load_next_session_page(state);
        }
        for _ in 0..steps {
            if upward {
                state.select_previous_session();
            } else {
                state.select_next_session();
            }
        }
        return;
    }
    if let Some(menu) = state.slash_menu.as_mut() {
        for _ in 0..steps {
            match menu.sub_menu.as_mut() {
                Some(sub) => {
                    if upward {
                        sub.selected = sub.selected.saturating_sub(1);
                    } else if sub.selected + 1 < sub.items.len() {
                        sub.selected += 1;
                    }
                }
                None => {
                    if upward {
                        menu.selected = menu.selected.saturating_sub(1);
                    } else if menu.selected + 1 < menu.items.len() {
                        menu.selected += 1;
                    }
                }
            }
        }
        return;
    }
    if state.mention.phase.is_some() && !state.mention.candidates.is_empty() {
        for _ in 0..steps {
            if upward {
                state.mention.selected = state.mention.selected.saturating_sub(1);
            } else {
                let max = state.mention.candidates.len().saturating_sub(1);
                if state.mention.selected < max {
                    state.mention.selected += 1;
                }
            }
        }
        crate::mention_menu_actions::mark_manual_selection(state);
        return;
    }
    match state.panel_mode {
        PanelMode::Workflows => {
            for _ in 0..steps {
                if upward {
                    state.select_previous_workflow_task();
                } else {
                    state.select_next_workflow_task();
                }
            }
        }
        PanelMode::Agents => {
            for _ in 0..steps {
                if upward {
                    state.select_previous_agent();
                } else {
                    state.select_next_agent();
                }
            }
        }
        PanelMode::Conversation => {
            if !state.accepts_mouse_scroll_at(now) {
                return;
            }
            if upward {
                state.scroll_up(lines.unsigned_abs().min(u16::MAX as u32) as u16);
            } else {
                state.scroll_down((lines as u32).min(u16::MAX as u32) as u16);
            }
        }
    }
}

pub(crate) fn handle_paste_event(
    ev: &Event,
    state: &mut AppState,
    config: &RunConfig,
    action_tx: &mpsc::Sender<crate::protocol::UserAction>,
    textarea: &mut TextArea,
) -> bool {
    let Event::Paste(pasted) = ev else {
        return false;
    };
    if state.config_dialog.is_some() || state.full_access_confirmation.is_some() {
        return true;
    }
    if let Some(dialog) = state.user_input_dialog.as_mut() {
        return dialog.insert_paste(pasted);
    }
    if state.transcript.search.open {
        state.transcript.search.insert_paste(pasted);
        state.refresh_transcript_search();
        return true;
    }
    match state.status {
        AppStatus::Setup if state.setup_step == 1 => {
            insert_pasted_text(textarea, pasted);
        }
        AppStatus::Idle | AppStatus::Running => {
            if let Some(paths) = image_paths_from_paste(pasted) {
                return begin_image_paste(state, action_tx, ImagePasteRequest::Paths(paths));
            }
            if insert_composer_paste(textarea, &mut state.pending_pastes, pasted) {
                state.reset_history_navigation();
                refresh_input_menus(textarea, state, config);
            }
        }
        AppStatus::WaitingUserInput => {
            if insert_composer_paste(textarea, &mut state.pending_pastes, pasted) {
                state.reset_history_navigation();
                refresh_input_menus(textarea, state, config);
            }
        }
        _ => {}
    }
    true
}

/// How a mouse event was consumed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MouseFlow {
    /// Not a mouse event at all; fall through to key handling.
    NotMouse,
    /// Consumed by the mouse layer.
    Handled,
    /// A click confirmed the focused list row (approval option, session,
    /// menu item). The caller should run the same path a real Enter takes.
    SyntheticEnter,
}

pub(crate) fn handle_mouse_event(
    ev: &Event,
    state: &mut AppState,
    textarea: &mut TextArea,
    now: Instant,
) -> MouseFlow {
    let Event::Mouse(mouse) = ev else {
        return MouseFlow::NotMouse;
    };
    if let Some(viewer) = state.image_viewer.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollUp => viewer.zoom_in(),
            MouseEventKind::ScrollDown => viewer.zoom_out(),
            _ => {}
        }
        state.viewport.selection = None;
        return MouseFlow::Handled;
    }
    if state.config_dialog.is_some()
        || state.full_access_confirmation.is_some()
        || state.user_input_dialog.is_some()
    {
        state.viewport.selection = None;
        return MouseFlow::Handled;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let lines = if mouse.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            };
            handle_scroll_lines(state, lines, now);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            const MULTI_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
            state.viewport.drag_edge_scroll = None;

            if !state.show_shortcuts
                && state.plan_approval_dialog.is_none()
                && state.status != AppStatus::WaitingApproval
                && state.panel_mode == PanelMode::Conversation
                && let Some(image) = state
                    .image_hit_areas
                    .iter()
                    .find(|hit| {
                        hit.rect
                            .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                    })
                    .map(|hit| hit.image.clone())
            {
                state.viewport.selection = None;
                match crate::image_preview::ImageViewerState::open(image) {
                    Ok(viewer) => state.image_viewer = Some(viewer),
                    Err(error) => {
                        state.push_message(crate::transcript_state::ChatMessage::Error(error))
                    }
                }
                return MouseFlow::Handled;
            }

            if state.plan_approval_dialog.is_some() {
                state.viewport.selection = None;
                if let Some(index) =
                    crate::ui::plan_approval_option_hit_index(state, mouse.column, mouse.row)
                    && let Some(dialog) = state.plan_approval_dialog.as_mut()
                {
                    if dialog.selected == index {
                        return MouseFlow::SyntheticEnter;
                    }
                    dialog.selected = index;
                }
                return MouseFlow::Handled;
            }

            // Modal approval dialog: clicks select options, a click on the
            // already-selected option confirms it, anywhere else is inert.
            // (Two-step so a stray click can never approve a command.)
            if state.status == AppStatus::WaitingApproval {
                state.viewport.selection = None;
                if let Some(index) =
                    crate::ui::approval_option_hit_index(state, mouse.column, mouse.row)
                    && let Some(dialog) = state.approval_dialog.as_mut()
                {
                    if dialog.selected == index {
                        return MouseFlow::SyntheticEnter;
                    }
                    dialog.selected = index;
                }
                return MouseFlow::Handled;
            }

            // Session picker: click selects, click on the selection resumes.
            if state.status == AppStatus::SessionPicker {
                if state.session_picker_phase != SessionPickerPhase::Browsing {
                    return MouseFlow::Handled;
                }
                if let Some(index) = crate::ui::session_picker_hit_index(state, mouse.row) {
                    if state.session_picker_selected == index {
                        return MouseFlow::SyntheticEnter;
                    }
                    state.session_picker_selected = index;
                }
                return MouseFlow::Handled;
            }

            // Popup menus over the composer: click selects, click on the
            // selection accepts. Clicks outside their popups fall through.
            // (The hit-tests are mutually exclusive: the mention popup only
            // renders — and only hits — while no slash menu is open.)
            if state.slash_menu.is_some()
                && let Some(index) = crate::ui::slash_menu_hit_index(state, mouse.column, mouse.row)
            {
                let menu = state.slash_menu.as_mut().expect("checked above");
                let selected = match menu.sub_menu.as_mut() {
                    Some(sub) => {
                        let confirm = sub.selected == index;
                        sub.selected = index;
                        confirm
                    }
                    None => {
                        let confirm = menu.selected == index;
                        menu.selected = index;
                        confirm
                    }
                };
                if selected {
                    return MouseFlow::SyntheticEnter;
                }
                return MouseFlow::Handled;
            }
            if state.mention.phase.is_some()
                && let Some(index) =
                    crate::ui::mention_menu_hit_index(state, mouse.column, mouse.row)
            {
                let confirm = state.mention.selected == index;
                state.mention.selected = index;
                crate::mention_menu_actions::mark_manual_selection(state);
                if confirm {
                    return MouseFlow::SyntheticEnter;
                }
                return MouseFlow::Handled;
            }

            // Composer: a click moves the cursor and starts a (potential)
            // in-composer drag selection.
            if matches!(
                state.status,
                AppStatus::Idle | AppStatus::WaitingUserInput | AppStatus::Setup
            ) && let Some(area) = state.viewport.input_area
                && let Some((row, col)) =
                    crate::ui::composer_click_target(textarea, area, mouse.column, mouse.row)
            {
                textarea.cancel_selection();
                textarea.move_cursor(tui_textarea::CursorMove::Jump(row, col));
                let text = crate::composer_textarea::textarea_text(textarea);
                let cursor = crate::composer_textarea::textarea_cursor_byte_index(textarea);
                if let Some(image) = state
                    .composer_images
                    .activatable_preview_at_cursor(&text, cursor)
                {
                    match crate::image_preview::ImageViewerState::open(image) {
                        Ok(viewer) => state.image_viewer = Some(viewer),
                        Err(error) => {
                            state.push_message(crate::transcript_state::ChatMessage::Error(error));
                        }
                    }
                    state.viewport.composer_mouse_selecting = false;
                    state.viewport.last_left_click = None;
                    return MouseFlow::Handled;
                }
                textarea.start_selection();
                state.viewport.composer_mouse_selecting = true;
                state.viewport.last_left_click = None;
                return MouseFlow::Handled;
            }

            // The floating "jump to bottom" pill eats the click before any
            // selection handling: re-arm follow instead of starting a drag.
            if state.panel_mode == PanelMode::Conversation
                && let Some(pill) = state.viewport.jump_to_bottom_area
                && pill.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
            {
                state.scroll_to_bottom();
                state.viewport.last_left_click = None;
                return MouseFlow::Handled;
            }

            // Click-count state machine with one cell of jitter tolerance
            // (trackpads rarely double-click on exactly the same cell).
            // Single → cell drag, double → word, triple → line; a fourth
            // quick click cycles back to a plain single click.
            let count = match state.viewport.last_left_click {
                Some((at, column, row, previous))
                    if now.duration_since(at) <= MULTI_CLICK_WINDOW
                        && column.abs_diff(mouse.column) <= 1
                        && row.abs_diff(mouse.row) <= 1 =>
                {
                    previous % 3 + 1
                }
                _ => 1,
            };
            state.viewport.last_left_click = Some((now, mouse.column, mouse.row, count));

            // A single click on a collapsed tool-output or reasoning row
            // expands (or re-collapses) just that message instead of
            // starting a text selection. Only a plain single click does
            // this: double/triple clicks are the word/line selection
            // gestures below and must keep working even when they land on a
            // collapsible row, so `count == 1` is checked before the hit
            // areas ever come into it. Each `CollapsibleHitArea.rect` is
            // already clipped to the transcript area by `render_live_messages`,
            // so a click outside it (or outside `PanelMode::Conversation`
            // entirely) can never resolve to a message here.
            //
            // Same guard as the image hit-area click below. The shortcuts
            // overlay and the plan-approval dialog draw on top of a
            // still-rendered transcript instead of replacing it (both use a
            // floating `Clear`-then-redraw popup over `frame.area()`), so
            // the hit areas underneath stay live unless a click is
            // explicitly barred from reaching them. `WaitingApproval`
            // doesn't share that rendering quirk — `render_approval_panel`
            // replaces the composer chunk, not the transcript — but is
            // barred too, matching the image click's existing precedent of
            // deferring every click to a pending decision.
            if count == 1
                && !state.show_shortcuts
                && state.plan_approval_dialog.is_none()
                && state.status != AppStatus::WaitingApproval
                && state.panel_mode == PanelMode::Conversation
                && let Some(area) = state
                    .collapsible_hit_areas
                    .iter()
                    .find(|area| {
                        area.rect
                            .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                    })
                    .copied()
            {
                state.toggle_expandable_at(area.message_index);
                state.viewport.selection = None;
                return MouseFlow::Handled;
            }

            state.viewport.selection = if state.panel_mode == PanelMode::Conversation {
                let pos = state.transcript_pos_at(mouse.column, mouse.row);
                match (count, pos) {
                    (2, Some(pos)) => state
                        .selection_word_bounds(pos)
                        .map(|(start, end)| {
                            TranscriptSelection::unit(SelectionGranularity::Word, start, end)
                        })
                        .or(Some(TranscriptSelection::begin(pos))),
                    (3, Some(pos)) => state
                        .selection_line_bounds(pos)
                        .map(|(start, end)| {
                            TranscriptSelection::unit(SelectionGranularity::Line, start, end)
                        })
                        .or(Some(TranscriptSelection::begin(pos))),
                    (_, Some(pos)) => Some(TranscriptSelection::begin(pos)),
                    (_, None) => None,
                }
            } else {
                None
            };
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if state.viewport.composer_mouse_selecting {
                if let Some(area) = state.viewport.input_area {
                    // Clamp into the composer so the drag keeps tracking.
                    let column = mouse
                        .column
                        .clamp(area.x, area.x + area.width.saturating_sub(1).max(1));
                    let row = mouse
                        .row
                        .clamp(area.y, area.y + area.height.saturating_sub(1).max(1));
                    if let Some((row, col)) =
                        crate::ui::composer_click_target(textarea, area, column, row)
                    {
                        textarea.move_cursor(tui_textarea::CursorMove::Jump(row, col));
                    }
                }
                return MouseFlow::Handled;
            }
            if state.panel_mode == PanelMode::Conversation {
                // Dragging onto (or past) the first/last transcript row
                // scrolls, so the selection can grow beyond the visible
                // screen. The top edge must trigger on the row itself: the
                // transcript usually starts at y=0, so "above the area"
                // does not exist there.
                if let Some(area) = state.viewport.transcript_area {
                    if mouse.row <= area.y {
                        state.scroll_up(1usize);
                        state.viewport.drag_edge_scroll = Some((-1, mouse.column));
                    } else if mouse.row >= area.y.saturating_add(area.height).saturating_sub(1) {
                        state.scroll_down(1usize);
                        state.viewport.drag_edge_scroll = Some((1, mouse.column));
                    } else {
                        state.viewport.drag_edge_scroll = None;
                    }
                }
                let pos = state.transcript_pos_at_clamped(mouse.column, mouse.row);
                let dragging = state
                    .viewport
                    .selection
                    .filter(|selection| selection.dragging);
                if let (Some(mut selection), Some(pos)) = (dragging, pos) {
                    match selection.granularity {
                        SelectionGranularity::Cell => selection.head = pos,
                        SelectionGranularity::Word => {
                            let unit = state.selection_word_bounds(pos);
                            selection.extend_to_unit(pos, unit);
                        }
                        SelectionGranularity::Line => {
                            let unit = state.selection_line_bounds(pos);
                            selection.extend_to_unit(pos, unit);
                        }
                    }
                    state.viewport.selection = Some(selection);
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.viewport.drag_edge_scroll = None;
            if state.viewport.composer_mouse_selecting {
                state.viewport.composer_mouse_selecting = false;
                // A click without a drag leaves no active selection behind.
                if textarea
                    .selection_range()
                    .is_none_or(|(start, end)| start == end)
                {
                    textarea.cancel_selection();
                }
                return MouseFlow::Handled;
            }
            if let Some(selection) = state.viewport.selection.filter(|sel| sel.dragging) {
                let mut settled = selection;
                settled.dragging = false;
                // A plain single click selects nothing; word/line units are
                // legitimate selections even when anchor == head.
                let plain_click =
                    settled.granularity == SelectionGranularity::Cell && settled.is_empty();
                if plain_click {
                    state.viewport.selection = None;
                } else {
                    state.viewport.selection = Some(settled);
                    let text = state.extract_selection_text(&settled);
                    if !text.is_empty() {
                        state.stage_clipboard_copy(text, now);
                    }
                }
            }
        }
        _ => {}
    }
    MouseFlow::Handled
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use tui_textarea::TextArea;

    use super::{
        BatchedInputEvent, MouseFlow, coalesce_input_events, handle_resize_event,
        handle_scroll_lines, should_queue_input_event,
    };
    use crate::protocol::UserAction;
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::transcript_view::TranscriptRenderContext;
    use crate::types::{AppState, AppStatus, ApprovalDialog, ApprovalOption, PlanApprovalDialog};

    /// Test shim: most cases don't care about the composer, so route the real
    /// handler through a throwaway textarea.
    fn handle_mouse_event(ev: &Event, state: &mut AppState, now: Instant) -> MouseFlow {
        let mut textarea = TextArea::default();
        super::handle_mouse_event(ev, state, &mut textarea, now)
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// An AppState with one transcript message ("hello world") rendered into
    /// a 20x5 transcript area at the screen origin, scrolled to the top.
    fn state_with_transcript() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded::<UserAction>();
        let mut state = AppState::new(
            tx,
            "0.0.0".to_string(),
            "model".to_string(),
            "cwd".to_string(),
        );
        state.push_message(ChatMessage::System {
            text: "seed".to_string(),
            expanded: false,
        });
        state.reconcile_message_tracking();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript.render_cache.prepare(
            &state.transcript.messages,
            &state.transcript.message_revisions,
            TranscriptRenderContext::new(&theme, 20, 0, false),
            |_, _, _, _, _, _| vec![Line::from("hello world")],
        );
        state.viewport.transcript_area = Some(Rect::new(0, 0, 20, 5));
        state.viewport.viewport_base_row = 0;
        state
    }

    /// A fresh `AppState` with no messages yet, for tests that need a real
    /// `crate::ui::render` pass (via `render_once`) rather than a hand-primed
    /// render cache like `state_with_transcript`.
    fn test_state() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded::<UserAction>();
        AppState::new(
            tx,
            "0.0.0".to_string(),
            "model".to_string(),
            "cwd".to_string(),
        )
    }

    /// Matches `ui.rs`'s test `tool_call` helper field-for-field.
    fn tool_call(
        name: &str,
        target: Option<&str>,
        status: &str,
        output: Option<&str>,
        expanded: bool,
    ) -> ChatMessage {
        ChatMessage::ToolCall {
            id: "call-1".into(),
            name: name.into(),
            target: target.map(str::to_string),
            status: status.into(),
            output: output.map(str::to_string),
            diff: None,
            kind: None,
            expanded,
        }
    }

    /// Renders one real frame at `width`x`height`, exactly as the render
    /// loop would: populates `state.viewport.transcript_area`,
    /// `state.collapsible_hit_areas`, and everything else a click handler
    /// reads. Mirrors `image_preview.rs`'s
    /// `message_thumbnail_registers_a_clickable_hit_area` test.
    fn render_once(state: &mut AppState, width: u16, height: u16) {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test backend");
        terminal
            .draw(|frame| crate::ui::render(frame, state, &textarea, &theme))
            .expect("draw");
    }

    /// Presses and releases the left mouse button at `(column, row)` in one
    /// click, returning whether the press was consumed by
    /// `handle_mouse_event`.
    fn click_at(state: &mut AppState, column: u16, row: u16) -> bool {
        let now = Instant::now();
        let handled = handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), column, row),
            state,
            now,
        ) == MouseFlow::Handled;
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), column, row),
            state,
            now,
        );
        handled
    }

    #[test]
    fn clicking_a_collapsed_tool_row_expands_only_that_message() {
        let mut state = test_state();
        state.push_message(tool_call(
            "bash",
            Some("a"),
            "completed",
            Some("l1\nl2\nl3\nl4"),
            false,
        ));
        state.push_message(tool_call(
            "bash",
            Some("b"),
            "completed",
            Some("m1\nm2\nm3\nm4"),
            false,
        ));
        render_once(&mut state, 100, 30);
        let area = state.collapsible_hit_areas[0];

        let handled = click_at(&mut state, area.rect.x + 2, area.rect.y);

        assert!(handled);
        assert!(matches!(
            &state.transcript.messages[0],
            ChatMessage::ToolCall { expanded: true, .. }
        ));
        assert!(matches!(
            &state.transcript.messages[1],
            ChatMessage::ToolCall {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn a_click_while_the_shortcuts_overlay_is_open_does_not_toggle_a_collapsible_row_underneath() {
        // `render_shortcuts` (ui.rs) draws the help overlay on top of a
        // still-rendered transcript, so `collapsible_hit_areas` stays
        // populated underneath it. The overlay owns the click, same as it
        // already owns Esc (`key_event_actions.rs`'s `show_shortcuts` guard).
        let mut state = test_state();
        state.push_message(tool_call(
            "bash",
            Some("a"),
            "completed",
            Some("l1\nl2\nl3\nl4"),
            false,
        ));
        render_once(&mut state, 100, 30);
        let area = state.collapsible_hit_areas[0];
        state.show_shortcuts = true;

        click_at(&mut state, area.rect.x + 2, area.rect.y);

        assert!(
            matches!(
                &state.transcript.messages[0],
                ChatMessage::ToolCall {
                    expanded: false,
                    ..
                }
            ),
            "the shortcuts overlay must own the click, not the row underneath it"
        );
        assert!(state.show_shortcuts, "the overlay itself stays open");
    }

    #[test]
    fn a_press_inside_expanded_output_starts_a_selection_instead_of_collapsing_it() {
        // Only a collapsible message's header and `└ ` tail rows toggle it.
        // The rows in between are content: a press there begins the ordinary
        // text selection, so output can be dragged over and copied.
        let mut state = test_state();
        state.push_message(tool_call(
            "bash",
            Some("cargo test"),
            "completed",
            Some("error one\nerror two\nerror three\nerror four"),
            true,
        ));
        render_once(&mut state, 100, 30);
        let transcript = state.viewport.transcript_area.expect("transcript");
        let header = state.collapsible_hit_areas[0].rect.y;

        handle_mouse_event(
            &mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                transcript.x + 8,
                header + 2,
            ),
            &mut state,
            Instant::now(),
        );

        assert!(
            matches!(
                &state.transcript.messages[0],
                ChatMessage::ToolCall { expanded: true, .. }
            ),
            "a press on an output row must not collapse the output"
        );
        assert!(
            state
                .viewport
                .selection
                .is_some_and(|selection| selection.dragging),
            "the press begins a drag selection"
        );
    }

    #[test]
    fn only_the_header_and_tail_rows_of_a_collapsed_tool_toggle_it() {
        let mut state = test_state();
        state.push_message(tool_call(
            "bash",
            Some("a"),
            "completed",
            Some("l1\nl2\nl3\nl4"),
            false,
        ));
        render_once(&mut state, 100, 30);
        let rows = state
            .collapsible_hit_areas
            .iter()
            .map(|area| (area.rect.y, area.rect.height))
            .collect::<Vec<_>>();
        let header = rows[0].0;
        assert_eq!(
            rows,
            [(header, 1), (header + 3, 1)],
            "the header and the `└ +2 lines` tail; the two preview rows stay selectable"
        );

        let tail = state.collapsible_hit_areas[1];
        assert!(click_at(&mut state, tail.rect.x + 2, tail.rect.y));
        assert!(matches!(
            &state.transcript.messages[0],
            ChatMessage::ToolCall { expanded: true, .. }
        ));
    }

    #[test]
    fn a_click_that_hits_no_collapsible_row_still_starts_a_text_selection() {
        let mut state = test_state();
        state.push_message(ChatMessage::Assistant("plain paragraph".into()));
        render_once(&mut state, 100, 30);
        let transcript = state.viewport.transcript_area.expect("transcript");

        // Check the press alone, like `drag_selects_and_release_stages_a_clipboard_copy`
        // does: a plain click's release (no drag) legitimately clears an empty
        // cell selection back to `None` (`plain_click_clears_selection_without_copying`),
        // so asserting post-release here would conflate that with a regression.
        handle_mouse_event(
            &mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                transcript.x + 1,
                transcript.y,
            ),
            &mut state,
            Instant::now(),
        );

        assert!(
            state.viewport.selection.is_some(),
            "a click that misses every collapsible hit area must fall through \
             to the ordinary text-selection gesture"
        );
    }

    #[test]
    fn a_click_below_the_transcript_area_does_not_toggle_a_collapsible_row() {
        // Carried from Task 4's review: `message_index_at_row` alone has no
        // upper bound on `row`, so this pins that the click handler itself
        // never resolves a row outside the drawn transcript to a message —
        // whether that guarantee comes from confining to the transcript
        // rect explicitly or (as implemented) from every `CollapsibleHitArea`
        // already being clipped to it by `render_live_messages`.
        let mut state = test_state();
        state.push_message(tool_call(
            "bash",
            Some("a"),
            "completed",
            Some("l1\nl2\nl3\nl4"),
            false,
        ));
        render_once(&mut state, 100, 30);
        let transcript = state.viewport.transcript_area.expect("transcript");
        let below = transcript.y + transcript.height;

        click_at(&mut state, transcript.x + 2, below);

        assert!(matches!(
            &state.transcript.messages[0],
            ChatMessage::ToolCall {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn double_clicking_a_collapsible_row_toggles_once_then_selects_a_word() {
        // The one way to silently break word/line selection would be to
        // intercept every click that lands on a collapsible row, not just
        // single clicks. Pin that a double click on such a row still behaves
        // like `double_click_selects_the_word_and_copies_immediately`: only
        // the first (single) press toggles, the second press selects.
        let mut state = test_state();
        state.push_message(tool_call(
            "read_file",
            Some("src/main.rs"),
            "completed",
            Some("l1\nl2\nl3\nl4"),
            false,
        ));
        render_once(&mut state, 100, 30);
        let area = state.collapsible_hit_areas[0];

        assert!(click_at(&mut state, area.rect.x + 2, area.rect.y));
        assert!(matches!(
            &state.transcript.messages[0],
            ChatMessage::ToolCall { expanded: true, .. }
        ));
        assert_eq!(
            state.viewport.selection, None,
            "toggling a row must not also start a selection"
        );

        click_at(&mut state, area.rect.x + 2, area.rect.y);

        assert!(
            matches!(
                &state.transcript.messages[0],
                ChatMessage::ToolCall { expanded: true, .. }
            ),
            "the second press of a double click must not toggle the row shut again"
        );
        assert!(
            state.viewport.selection.is_some(),
            "the second press of a double click must select a word, same as any other row"
        );
    }

    #[test]
    fn drag_selects_and_release_stages_a_clipboard_copy() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 0),
            &mut state,
            now,
        );
        assert!(state.viewport.selection.is_some_and(|sel| sel.dragging));

        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );

        assert!(state.viewport.selection.is_some_and(|sel| !sel.dragging));
        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("world")
        );
    }

    #[test]
    fn plain_click_clears_selection_without_copying() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );

        assert_eq!(state.viewport.selection, None);
        assert_eq!(state.viewport.pending_clipboard_copy, None);
    }

    #[test]
    fn press_outside_the_transcript_dismisses_the_selection() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        assert!(state.viewport.selection.is_some());

        // Next press lands below the transcript area (row 30).
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 30),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.selection, None);
    }

    #[test]
    fn drag_beyond_the_area_clamps_to_the_nearest_cell() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 50, 50),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 50, 50),
            &mut state,
            now,
        );

        // Clamped to the area's bottom-right cell; extraction still stops at
        // the actual content, so the whole line is copied.
        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn wheel_events_still_scroll_and_do_not_touch_selection() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(&mouse(MouseEventKind::ScrollUp), &mut state, now);
        assert_eq!(state.viewport.selection, None);
        assert_eq!(state.viewport.pending_clipboard_copy, None);
    }

    #[test]
    fn double_click_selects_the_word_and_copies_immediately() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        for _ in 0..2 {
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
            handle_mouse_event(
                &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
        }

        assert!(state.viewport.selection.is_some_and(|sel| !sel.dragging));
        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("world")
        );
    }

    #[test]
    fn slow_second_click_does_not_word_select() {
        let mut state = state_with_transcript();
        let first = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &mut state,
            first,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            first,
        );
        let later = first + std::time::Duration::from_millis(800);
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &mut state,
            later,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            later,
        );

        assert_eq!(state.viewport.selection, None);
        assert_eq!(state.viewport.pending_clipboard_copy, None);
    }

    #[test]
    fn dragging_past_the_edges_scrolls_the_transcript() {
        let mut state = state_with_transcript();
        // Pretend the transcript overflows the area so scrolling is possible.
        state.viewport.total_lines = 50;
        state.viewport.visible_height = 5;
        state.viewport.scroll_offset = 10;
        state.viewport.auto_scroll = false;
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        // Dragging below the bottom edge scrolls down...
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 40),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.scroll_offset, 11);
        // ...and dragging onto the TOP ROW scrolls up. The transcript area
        // starts at y=0, so there is no "above the area" — the first row
        // itself must trigger.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.scroll_offset, 10);
    }

    #[test]
    fn parked_pointer_at_the_edge_keeps_scrolling_via_animation_ticks() {
        let mut state = state_with_transcript();
        state.viewport.total_lines = 50;
        state.viewport.visible_height = 5;
        state.viewport.scroll_offset = 10;
        state.viewport.auto_scroll = false;
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        // Reaching the top row arms edge auto-scroll (and scrolls once).
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.drag_edge_scroll, Some((-1, 3)));
        assert_eq!(state.viewport.scroll_offset, 9);
        let head_before = state.viewport.selection.unwrap().head;

        // With the pointer parked (no further mouse events), animation ticks
        // keep scrolling and keep growing the selection upward.
        state.apply_drag_edge_scroll();
        state.apply_drag_edge_scroll();
        assert_eq!(state.viewport.scroll_offset, 7);
        let head_after = state.viewport.selection.unwrap().head;
        assert_eq!(head_after.row, head_before.row.saturating_sub(2));

        // Dragging back inside the area disarms it...
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.drag_edge_scroll, None);

        // ...and so does releasing the button at the edge.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert!(state.viewport.drag_edge_scroll.is_some());
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.viewport.drag_edge_scroll, None);
        // A settled (non-dragging) selection is no longer grown by ticks.
        let settled_head = state.viewport.selection.unwrap().head;
        state.apply_drag_edge_scroll();
        assert_eq!(state.viewport.selection.unwrap().head, settled_head);
    }

    #[test]
    fn clicking_the_jump_pill_rearms_follow_instead_of_selecting() {
        let mut state = state_with_transcript();
        state.viewport.total_lines = 50;
        state.viewport.visible_height = 5;
        state.viewport.scroll_offset = 10;
        state.viewport.auto_scroll = false;
        state.viewport.jump_to_bottom_area = Some(Rect::new(5, 4, 10, 1));
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 7, 4),
            &mut state,
            now,
        );

        assert!(state.viewport.auto_scroll);
        assert_eq!(state.viewport.scroll_offset, 45);
        // The click was consumed by the pill: no selection was started.
        assert_eq!(state.viewport.selection, None);

        // A click elsewhere still starts a selection as usual.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert!(state.viewport.selection.is_some());
    }

    #[test]
    fn adjacent_wheel_events_collapse_to_a_signed_line_delta() {
        let events = vec![
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::ScrollDown),
        ];

        assert_eq!(
            coalesce_input_events(events, 3),
            vec![
                BatchedInputEvent::ScrollLines(-6),
                BatchedInputEvent::ScrollLines(3),
            ]
        );
    }

    #[test]
    fn non_wheel_events_preserve_order_and_split_scroll_runs() {
        let resize = Event::Resize(120, 40);
        let key = Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let events = vec![
            mouse(MouseEventKind::ScrollUp),
            resize.clone(),
            mouse(MouseEventKind::ScrollDown),
            key.clone(),
        ];

        assert_eq!(
            coalesce_input_events(events, 3),
            vec![
                BatchedInputEvent::ScrollLines(-3),
                BatchedInputEvent::Event(resize),
                BatchedInputEvent::ScrollLines(3),
                BatchedInputEvent::Event(key),
            ]
        );
    }

    #[test]
    fn pointer_motion_events_are_dropped_at_intake() {
        assert!(!should_queue_input_event(&mouse(MouseEventKind::Moved)));
        assert!(should_queue_input_event(&mouse(MouseEventKind::Drag(
            MouseButton::Left
        ))));
        assert!(should_queue_input_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE
        ))));
    }

    #[test]
    fn resize_invalidates_the_selection() {
        let mut state = state_with_transcript();
        let now = Instant::now();
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 8, 0),
            &mut state,
            now,
        );
        assert!(state.viewport.selection.is_some());

        assert!(handle_resize_event(&Event::Resize(100, 40), &mut state));
        assert_eq!(state.viewport.selection, None);
        assert!(!handle_resize_event(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut state
        ));
    }

    #[test]
    fn double_click_tolerates_one_cell_of_jitter() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            now,
        );
        // One column over: still a double click.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 9, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 9, 0),
            &mut state,
            now,
        );
        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("world")
        );
    }

    #[test]
    fn triple_click_selects_the_logical_line() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        for _ in 0..3 {
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
            handle_mouse_event(
                &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
        }

        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn double_click_then_drag_extends_word_wise() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        // Double click lands on "hello" (cols 0-4).
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 2, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 2, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 2, 0),
            &mut state,
            now,
        );
        // Drag onto "world": the selection swallows both words.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 8, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            now,
        );

        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn approval_clicks_select_then_confirm_and_suppress_transcript_selection() {
        let mut state = state_with_transcript();
        state.status = AppStatus::WaitingApproval;
        state.viewport.frame_area = Some(Rect::new(0, 0, 80, 24));
        // The approval panel now renders in the composer slot, and its hit
        // test reads `input_area` (not `frame_area`) to match; the panel
        // fills whatever rect it is given, so a click test can hand it the
        // same full-frame rect the old centered-modal geometry used to
        // derive its popup from.
        state.viewport.input_area = Some(Rect::new(0, 0, 80, 24));
        state.approval_dialog = Some(ApprovalDialog {
            id: "1".to_string(),
            interaction: None,
            tool: "bash".to_string(),
            target: Some("ls".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ],
            diff: None,
        });
        let now = Instant::now();

        let geometry_probe = crate::ui::approval_option_hit_index(&state, 40, 0);
        assert_eq!(geometry_probe, None, "border row must not hit");

        // Find the actual first option row by probing.
        let first_option_row = (0..24)
            .find(|row| crate::ui::approval_option_hit_index(&state, 40, *row) == Some(0))
            .expect("dialog options must be hittable");

        // Click the third option: selects it, does not confirm.
        assert_eq!(
            handle_mouse_event(
                &mouse_at(
                    MouseEventKind::Down(MouseButton::Left),
                    40,
                    first_option_row + 2
                ),
                &mut state,
                now,
            ),
            MouseFlow::Handled
        );
        assert_eq!(
            state.approval_dialog.as_ref().map(|dialog| dialog.selected),
            Some(2)
        );
        // No transcript selection was started underneath the dialog.
        assert_eq!(state.viewport.selection, None);

        // Click it again: confirm via the synthetic Enter path.
        assert_eq!(
            handle_mouse_event(
                &mouse_at(
                    MouseEventKind::Down(MouseButton::Left),
                    40,
                    first_option_row + 2
                ),
                &mut state,
                now,
            ),
            MouseFlow::SyntheticEnter
        );
    }

    #[test]
    fn plan_approval_clicks_select_then_confirm() {
        let mut state = state_with_transcript();
        state.viewport.frame_area = Some(Rect::new(0, 0, 100, 30));
        state.plan_approval_dialog = Some(PlanApprovalDialog {
            plan: "- inspect\n- implement".to_string(),
            selected: 0,
        });
        let now = Instant::now();
        let second_row = (0..30)
            .find(|row| crate::ui::plan_approval_option_hit_index(&state, 50, *row) == Some(1))
            .expect("second plan option should be hittable");

        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 50, second_row),
                &mut state,
                now,
            ),
            MouseFlow::Handled
        );
        assert_eq!(
            state
                .plan_approval_dialog
                .as_ref()
                .map(|dialog| dialog.selected),
            Some(1)
        );
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 50, second_row),
                &mut state,
                now,
            ),
            MouseFlow::SyntheticEnter
        );
    }

    fn test_workflow_task(id: &str) -> orca_core::task_types::BackgroundTaskSummary {
        orca_core::task_types::BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type: orca_core::task_types::TaskType::Workflow,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: false,
            lifetime: orca_core::task_types::TaskLifetime::Task,
            description: id.to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(id.to_string()),
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
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }
    }

    #[test]
    fn wheel_routes_to_the_focused_list() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        // Workflows panel: wheel moves the task selection.
        state.panel_mode = crate::types::PanelMode::Workflows;
        state.replace_workflow_tasks_for_test(vec![
            test_workflow_task("a"),
            test_workflow_task("b"),
            test_workflow_task("c"),
        ]);
        state.select_workflow_index_for_test(0);
        handle_scroll_lines(&mut state, 3, now);
        assert_eq!(state.workflow_selected_index(), 1);
        handle_scroll_lines(&mut state, -3, now);
        assert_eq!(state.workflow_selected_index(), 0);

        // Agents panel: wheel moves the stable agent selection.
        let mut first_agent = test_workflow_task("agent-a");
        first_agent.task_type = orca_core::task_types::TaskType::Subagent;
        first_agent.created_at_ms = 1_000;
        let mut second_agent = test_workflow_task("agent-b");
        second_agent.task_type = orca_core::task_types::TaskType::Subagent;
        second_agent.created_at_ms = 2_000;
        state.replace_workflow_tasks_for_test(vec![first_agent, second_agent]);
        state.show_agents();
        handle_scroll_lines(&mut state, 3, now);
        assert_eq!(state.agent_selected_index(), 1);
        handle_scroll_lines(&mut state, -3, now);
        assert_eq!(state.agent_selected_index(), 0);

        // Session picker: wheel moves the session selection.
        state.panel_mode = crate::types::PanelMode::Conversation;
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = vec![test_session_summary("a"), test_session_summary("b")];
        state.session_picker_selected = 0;
        handle_scroll_lines(&mut state, 3, now);
        assert_eq!(state.session_picker_selected, 1);
    }

    fn test_session_summary(title: &str) -> orca_runtime::history::SessionSummary {
        orca_runtime::history::SessionSummary {
            session_id: title.to_string(),
            title: title.to_string(),
            cwd: ".".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            path: std::path::PathBuf::new(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
            health: orca_runtime::history::StoredSessionHealth::Healthy,
            health_issue: None,
            source_fingerprint: None,
            storage_identity: title.to_string(),
        }
    }

    #[test]
    fn session_picker_click_selects_then_resumes() {
        let mut state = state_with_transcript();
        state.status = AppStatus::SessionPicker;
        state.viewport.frame_area = Some(Rect::new(0, 0, 80, 24));
        // Same cwd, so both land in one picker group; pin distinct
        // `updated_at` values (rather than relying on two back-to-back
        // `Utc::now()` calls to land in call order) so "beta" deterministically
        // sorts newest-first ahead of "alpha".
        let mut alpha = test_session_summary("alpha");
        let mut beta = test_session_summary("beta");
        let reference_time = chrono::Utc::now();
        alpha.updated_at = reference_time - chrono::Duration::minutes(10);
        beta.updated_at = reference_time;
        state.session_picker_sessions = vec![alpha, beta];
        state.session_picker_selected = 0;
        let now = Instant::now();

        // Rows: border(0), query(1), hints(2), blank(3), group header(4),
        // beta [newest] (5), alpha (6).
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 5),
                &mut state,
                now,
            ),
            MouseFlow::Handled
        );
        assert_eq!(state.session_picker_selected, 1);
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 5),
                &mut state,
                now,
            ),
            MouseFlow::SyntheticEnter
        );
    }

    #[test]
    fn slash_menu_click_selects_then_accepts() {
        let mut state = state_with_transcript();
        state.viewport.frame_area = Some(Rect::new(0, 0, 60, 24));
        state.viewport.input_area = Some(Rect::new(0, 20, 60, 3));
        state.slash_menu = Some(crate::types::SlashMenu {
            items: vec![
                crate::types::SlashMenuItem {
                    command: "/help".to_string(),
                    description: "help".to_string(),
                },
                crate::types::SlashMenuItem {
                    command: "/model".to_string(),
                    description: "model".to_string(),
                },
            ],
            selected: 0,
            sub_menu: None,
        });
        let now = Instant::now();

        // Popup: 2 items + hint row + border = height 5, sits at rows
        // 15..20; content rows 16 (item 0), 17 (item 1), 18 (hint row).
        // `popup_geometry`'s `show_status` reserves that hint row whenever
        // the renderer does (see the whole-branch review's C1), so the hit
        // test and `render_slash_menu` must agree on it or a click lands on
        // the row above the one the user clicked.
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 17),
                &mut state,
                now,
            ),
            MouseFlow::Handled
        );
        assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(1));
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 17),
                &mut state,
                now,
            ),
            MouseFlow::SyntheticEnter
        );
    }

    #[test]
    fn slash_menu_click_on_the_hint_row_does_not_select_the_last_command() {
        // Regression guard for the exact user-visible failure mode in C1:
        // before the fix, a click on the trailing hint row resolved to the
        // last command (here, item 1) because the hit test believed the
        // popup was one row shorter than the one actually drawn.
        let mut state = state_with_transcript();
        state.viewport.frame_area = Some(Rect::new(0, 0, 60, 24));
        state.viewport.input_area = Some(Rect::new(0, 20, 60, 3));
        state.slash_menu = Some(crate::types::SlashMenu {
            items: vec![
                crate::types::SlashMenuItem {
                    command: "/help".to_string(),
                    description: "help".to_string(),
                },
                crate::types::SlashMenuItem {
                    command: "/model".to_string(),
                    description: "model".to_string(),
                },
            ],
            selected: 0,
            sub_menu: None,
        });
        let now = Instant::now();

        // Row 18 is the hint row (see the geometry note above): clicking it
        // must not change the selection or accept anything.
        assert_eq!(
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 18),
                &mut state,
                now,
            ),
            MouseFlow::Handled
        );
        assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(0));
    }

    #[test]
    fn composer_click_positions_cursor_and_drag_selects() {
        let mut state = state_with_transcript();
        // `composer_click_target` now always reserves the top/bottom rule rows
        // and the 3-column ` › ` prompt, so the outer input area needs 2 extra
        // rows (height 4, not 2) and every click lands 3 columns / 1 row past
        // where it would inside the old bordered composer.
        state.viewport.input_area = Some(Rect::new(0, 20, 40, 4));
        let mut textarea = TextArea::from(["hello world", "second line"]);
        let now = Instant::now();

        // Click on row 1, column 6 → cursor jumps there.
        let flow = super::handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 9, 22),
            &mut state,
            &mut textarea,
            now,
        );
        assert_eq!(flow, MouseFlow::Handled);
        assert_eq!(textarea.cursor(), (1, 6));
        assert!(state.viewport.composer_mouse_selecting);

        // Drag to column 11 on the same row: an in-composer selection forms.
        super::handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 14, 22),
            &mut state,
            &mut textarea,
            now,
        );
        super::handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 14, 22),
            &mut state,
            &mut textarea,
            now,
        );
        assert!(!state.viewport.composer_mouse_selecting);
        assert_eq!(
            textarea.selection_range(),
            Some(((1, 6), (1, 11))),
            "drag should leave the composer selection in place"
        );

        // A plain click (no drag) leaves no selection behind.
        super::handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 5, 21),
            &mut state,
            &mut textarea,
            now,
        );
        super::handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 5, 21),
            &mut state,
            &mut textarea,
            now,
        );
        assert_eq!(textarea.selection_range(), None);
        assert_eq!(textarea.cursor(), (0, 2));
    }
}
