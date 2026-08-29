use std::io;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::Backend;
use tui_textarea::TextArea;

use crate::frame_scheduler::{
    FrameScheduler, IterationEvent, IterationOutcome, run_event_loop_iteration,
};
use crate::presentation::resume_terminal_render;
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus};
use crate::ui;

pub(crate) struct RendererFrameOwner {
    scheduler: FrameScheduler,
}

impl RendererFrameOwner {
    pub(crate) fn new(
        initial_draw_at: Instant,
        frame_interval: Duration,
        animation_interval: Duration,
    ) -> Self {
        let mut scheduler =
            FrameScheduler::new(initial_draw_at, frame_interval, animation_interval);
        scheduler.did_draw(initial_draw_at);
        Self { scheduler }
    }

    pub(crate) fn prepare_iteration(
        &mut self,
        now: Instant,
        state: &mut AppState,
        presentation: &mut TerminalPresentation,
    ) -> Duration {
        if state.poll_edit_highlight_results() {
            self.scheduler.mark_dirty();
        }

        // Compute demand before clearing an expired notice so this iteration
        // still schedules the final redraw that removes it from the screen.
        let animation_active = state.status == AppStatus::Running
            || state.viewport.copy_notice.is_some()
            || state.viewport.drag_edge_scroll.is_some()
            || state.edit_highlight_needs_tick()
            || presentation.animation_active(state.status);
        if state.viewport.copy_notice.is_some() && state.copy_notice_at(now).is_none() {
            state.viewport.copy_notice = None;
        }
        if animation_active && self.scheduler.animation_due(now) {
            state.advance_tick();
            presentation.advance_tick();
            state.apply_drag_edge_scroll();
            self.scheduler.did_animate(now);
        }

        self.scheduler.poll_timeout(now, animation_active)
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.scheduler.mark_dirty();
    }

    pub(crate) fn resume<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        presentation: &mut TerminalPresentation,
    ) -> io::Result<()> {
        resume_terminal_render(terminal, &mut self.scheduler, presentation)
    }

    pub(crate) fn run_iteration<I, R, II, RI, Clock, Handler, E>(
        &mut self,
        input_events: II,
        runtime_events: RI,
        input_limit: usize,
        runtime_limit: usize,
        frame_time: Clock,
        handle_event: Handler,
    ) -> Result<IterationOutcome, E>
    where
        II: IntoIterator<Item = I>,
        RI: IntoIterator<Item = R>,
        Clock: FnOnce() -> Instant,
        Handler: FnMut(IterationEvent<I, R>) -> Result<Option<i32>, E>,
    {
        run_event_loop_iteration(
            &mut self.scheduler,
            input_events,
            runtime_events,
            input_limit,
            runtime_limit,
            frame_time,
            handle_event,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn present_iteration<B, CopyClipboard, WritePending>(
        &mut self,
        terminal: &mut Terminal<B>,
        presentation: &mut TerminalPresentation,
        state: &mut AppState,
        textarea: &TextArea<'_>,
        theme: &Theme,
        draw_at: Option<Instant>,
        copy_clipboard: CopyClipboard,
        write_pending: WritePending,
    ) -> io::Result<()>
    where
        B: Backend,
        CopyClipboard: FnOnce(&str),
        WritePending: FnOnce(&mut Terminal<B>, &mut TerminalPresentation, AppStatus),
    {
        if let Some(text) = state.viewport.pending_clipboard_copy.take() {
            copy_clipboard(&text);
        }
        write_pending(terminal, presentation, state.status);
        if let Some(draw_at) = draw_at {
            terminal.draw(|frame| ui::render(frame, state, textarea, theme))?;
            self.scheduler.did_draw(draw_at);
        }
        Ok(())
    }

    #[cfg(test)]
    fn should_draw_for_test(&self, now: Instant) -> bool {
        self.scheduler.should_draw(now)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use orca_core::config::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tui_textarea::TextArea;

    use super::RendererFrameOwner;
    use crate::protocol::{TuiEvent, UserAction};
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::types::{AppState, AppStatus};

    fn state() -> AppState {
        let (action_tx, _action_rx) = crossbeam_channel::unbounded::<UserAction>();
        AppState::new(
            action_tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        )
    }

    fn presentation() -> TerminalPresentation {
        TerminalPresentation::new(
            false,
            TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        )
    }

    fn inserted_source_line<'a>(
        lines: &'a [ratatui::text::Line<'static>],
        source: &str,
    ) -> &'a ratatui::text::Line<'static> {
        lines
            .iter()
            .find(|line| {
                line.to_string().contains(source)
                    && line
                        .spans
                        .first()
                        .is_some_and(|span| span.content.ends_with("+ "))
            })
            .unwrap_or_else(|| panic!("inserted source line containing {source:?}"))
    }

    const POLL_EDIT_DIFF: &str = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    fn state_with_pending_edit() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().expect("edit workspace");
        std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
        std::fs::write(directory.path().join("src/item.py"), "value = 2\n")
            .expect("post-edit file");
        let mut state = state();
        state.configure_syntax_highlighting(
            directory.path().to_path_buf(),
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        );
        state.update(TuiEvent::ToolRequested {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited src/item.py".to_string(),
            diff: Some(POLL_EDIT_DIFF.to_string()),
            kind: None,
        });
        assert!(state.edit_highlight_needs_tick());
        (directory, state)
    }

    fn ready_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;

        let job = runtime.pending_job("edit-1").expect("pending edit");
        let styles = crate::diff_highlight::RefinedDiffStyles::from([(
            1,
            vec![Span::styled(
                "value = 2",
                Style::default().fg(Color::Magenta),
            )],
        )]);
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Ready {
                    styles: Arc::new(styles),
                },
            }],
            disconnected: false,
        }
    }

    fn failed_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        let job = runtime.pending_job("edit-1").expect("pending edit");
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
            }],
            disconnected: false,
        }
    }

    fn stale_drain(
        runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        let mut job = runtime.pending_job("edit-1").expect("pending edit");
        job.message_revision = job.message_revision.saturating_add(1);
        crate::edit_highlight_worker::DrainResults {
            results: vec![crate::edit_highlight_worker::EditHighlightResult {
                job,
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
            }],
            disconnected: false,
        }
    }

    fn disconnected_drain(
        _runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        crate::edit_highlight_worker::DrainResults {
            results: Vec::new(),
            disconnected: true,
        }
    }

    #[test]
    fn expiring_copy_notice_still_admits_its_final_dirty_frame() {
        let started = Instant::now();
        let now = started + AppState::COPY_NOTICE_TTL;
        let mut owner = RendererFrameOwner::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        let mut state = state();
        let mut presentation = presentation();
        state.stage_clipboard_copy("copy me".to_string(), started);

        let state_tick_before = state.tick;
        let title_tick_before = presentation.title(AppStatus::Running);
        let timeout = owner.prepare_iteration(now, &mut state, &mut presentation);

        assert!(state.viewport.copy_notice.is_none());
        assert_eq!(
            state.tick, state_tick_before,
            "idle state tick remains idle"
        );
        assert_ne!(presentation.title(AppStatus::Running), title_tick_before);
        assert_eq!(timeout, Duration::ZERO);
        assert!(owner.should_draw_for_test(now));
    }

    #[test]
    fn presentation_completion_consumes_clipboard_and_draws_once() {
        let started = Instant::now();
        let draw_at = started + Duration::from_millis(16);
        let mut owner = RendererFrameOwner::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        owner.mark_dirty();
        let outcome = owner
            .run_iteration(
                std::iter::empty::<()>(),
                std::iter::empty::<()>(),
                usize::MAX,
                256,
                || draw_at,
                |_event| Ok::<Option<i32>, ()>(None),
            )
            .expect("frame iteration");
        assert_eq!(outcome.draw_at, Some(draw_at));

        let mut state = state();
        state.viewport.pending_clipboard_copy = Some("copy once".to_string());
        let mut presentation = presentation();
        let textarea = TextArea::default();
        let theme = Theme::named(ThemeName::Dark);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let copy_calls = Rc::clone(&calls);
        let pending_calls = Rc::clone(&calls);

        owner
            .present_iteration(
                &mut terminal,
                &mut presentation,
                &mut state,
                &textarea,
                &theme,
                outcome.draw_at,
                move |text| copy_calls.borrow_mut().push(format!("copy:{text}")),
                move |_terminal, _presentation, _status| {
                    pending_calls.borrow_mut().push("pending".to_string());
                },
            )
            .expect("present frame");

        assert_eq!(calls.borrow().as_slice(), ["copy:copy once", "pending"]);
        assert!(state.viewport.pending_clipboard_copy.is_none());
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() != " ")
        );
        assert!(!owner.should_draw_for_test(draw_at + Duration::from_millis(16)));
    }

    #[test]
    fn ready_edit_highlight_poll_marks_dirty_and_clears_pending_animation() {
        let (_directory, mut state) = state_with_pending_edit();
        let started = Instant::now();
        let mut owner = RendererFrameOwner::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        let mut presentation = presentation();
        state.set_edit_highlight_drain_for_test(Some(ready_drain));

        assert!(state.edit_highlight_needs_tick());
        owner.prepare_iteration(started, &mut state, &mut presentation);
        assert!(!state.edit_highlight_needs_tick());
        assert!(!owner.should_draw_for_test(started + Duration::from_millis(15)));
        assert!(owner.should_draw_for_test(started + Duration::from_millis(16)));
    }

    #[test]
    fn idle_ready_poll_schedules_actual_render_with_refined_styles_once() {
        let (_directory, mut state) = state_with_pending_edit();
        state.push_message(ChatMessage::System("stable".to_string()));
        assert_eq!(state.status, AppStatus::Idle);
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test backend");

        terminal
            .draw(|frame| crate::ui::render(frame, &mut state, &textarea, &theme))
            .expect("cold render");
        assert_eq!(state.transcript.render_cache.last_prepare_visited(), 2);
        let cold = state
            .transcript
            .render_cache
            .viewport(0, usize::MAX, usize::MAX);
        let cold_insert = inserted_source_line(&cold.lines, "value = 2");
        assert!(
            cold_insert
                .spans
                .iter()
                .all(|span| span.style.fg != Some(ratatui::style::Color::Magenta))
        );

        let revisions_before = state.transcript.message_revisions.clone();
        let started = Instant::now();
        let draw_at = started + Duration::from_millis(16);
        let mut owner = RendererFrameOwner::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        let mut presentation = presentation();
        state.set_edit_highlight_drain_for_test(Some(ready_drain));

        assert!(state.edit_highlight_needs_tick());
        owner.prepare_iteration(started, &mut state, &mut presentation);
        assert_ne!(state.transcript.message_revisions[0], revisions_before[0]);
        assert_eq!(state.transcript.message_revisions[1], revisions_before[1]);
        assert_eq!(state.pending_edit_highlight_count_for_test(), 0);
        assert!(!state.edit_highlight_needs_tick());

        let outcome = owner
            .run_iteration(
                std::iter::empty::<()>(),
                std::iter::empty::<()>(),
                usize::MAX,
                256,
                || draw_at,
                |_event| Ok::<Option<i32>, ()>(None),
            )
            .expect("refined frame iteration");
        owner
            .present_iteration(
                &mut terminal,
                &mut presentation,
                &mut state,
                &textarea,
                &theme,
                outcome.draw_at,
                |_| {},
                |_terminal, _presentation, _status| {},
            )
            .expect("refined render");
        assert_eq!(state.transcript.render_cache.last_prepare_visited(), 1);
        let warm = state
            .transcript
            .render_cache
            .viewport(0, usize::MAX, usize::MAX);
        let warm_insert = inserted_source_line(&warm.lines, "value = 2");
        assert_eq!(
            warm_insert
                .spans
                .iter()
                .filter(|span| span.style.fg == Some(ratatui::style::Color::Magenta))
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "value = 2"
        );

        let revisions_after = state.transcript.message_revisions.clone();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut state, &textarea, &theme))
            .expect("steady render");
        assert_eq!(state.transcript.render_cache.last_prepare_visited(), 0);
        assert_eq!(state.transcript.message_revisions, revisions_after);
        assert!(!owner.should_draw_for_test(draw_at + Duration::from_secs(1)));
    }

    #[test]
    fn failed_and_stale_edit_highlight_polls_do_not_mark_dirty() {
        for (drain, remains_pending) in [
            (
                failed_drain
                    as fn(
                        &mut crate::edit_highlight_worker::EditHighlightRuntime,
                    ) -> crate::edit_highlight_worker::DrainResults,
                false,
            ),
            (stale_drain, true),
        ] {
            let (_directory, mut state) = state_with_pending_edit();
            let started = Instant::now();
            let mut owner = RendererFrameOwner::new(
                started,
                Duration::from_millis(16),
                Duration::from_millis(80),
            );
            let mut presentation = presentation();
            state.set_edit_highlight_drain_for_test(Some(drain));

            owner.prepare_iteration(started, &mut state, &mut presentation);
            assert_eq!(state.edit_highlight_needs_tick(), remains_pending);
            assert!(!owner.should_draw_for_test(started + Duration::from_millis(16)));
        }
    }

    #[test]
    fn disconnected_edit_highlight_poll_drops_runtime_and_stops_pending_tick() {
        let (_directory, mut state) = state_with_pending_edit();
        let started = Instant::now();
        let mut owner = RendererFrameOwner::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        let mut presentation = presentation();
        state.set_edit_highlight_drain_for_test(Some(disconnected_drain));

        assert!(state.edit_highlight_needs_tick());
        owner.prepare_iteration(started, &mut state, &mut presentation);
        assert!(!state.edit_highlight_runtime_started_for_test());
        assert!(!state.edit_highlight_needs_tick());
        assert!(!owner.should_draw_for_test(started + Duration::from_millis(16)));
    }
}
