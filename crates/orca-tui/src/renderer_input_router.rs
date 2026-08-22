use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel as mpsc;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::input_event_actions::{
    BatchedInputEvent, MouseFlow, consume_focus_event, handle_mouse_event, handle_paste_event,
    handle_resize_event, handle_scroll_lines,
};
use crate::insert_escape::{
    PendingInsertEscapeRouting, flush_pending_insert_escape_before_non_key,
    resolve_pending_insert_escape_before_routing,
};
use crate::key_event_actions::{KeyEventFlow, handle_key_event_preflight};
use crate::status_key_actions::{StatusKeyFlow, handle_status_key};
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::{AppState, UserAction};
use crate::vim::VimState;

pub(crate) struct RendererInputRouter<'a, 'text> {
    state: &'a mut AppState,
    config: &'a mut RunConfig,
    shared_config: &'a Arc<Mutex<RunConfig>>,
    action_tx: &'a mpsc::Sender<UserAction>,
    preloaded_transcript: &'a Arc<Mutex<Option<SessionTranscript>>>,
    textarea: &'a mut TextArea<'text>,
    vim_state: &'a mut VimState,
    theme: &'a Theme,
    presentation: &'a mut TerminalPresentation,
    initial_prompt: &'a Option<String>,
}

impl<'a, 'text> RendererInputRouter<'a, 'text> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        state: &'a mut AppState,
        config: &'a mut RunConfig,
        shared_config: &'a Arc<Mutex<RunConfig>>,
        action_tx: &'a mpsc::Sender<UserAction>,
        preloaded_transcript: &'a Arc<Mutex<Option<SessionTranscript>>>,
        textarea: &'a mut TextArea<'text>,
        vim_state: &'a mut VimState,
        theme: &'a Theme,
        presentation: &'a mut TerminalPresentation,
        initial_prompt: &'a Option<String>,
    ) -> Self {
        Self {
            state,
            config,
            shared_config,
            action_tx,
            preloaded_transcript,
            textarea,
            vim_state,
            theme,
            presentation,
            initial_prompt,
        }
    }

    pub(crate) fn route(
        mut self,
        input: BatchedInputEvent,
        now: Instant,
        mut clear_terminal: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<i32>> {
        match input {
            BatchedInputEvent::ScrollLines(lines) => {
                if self.state.config_dialog.is_some() {
                    self.vim_state.cancel_pending_command();
                    return Ok(None);
                }
                flush_pending_insert_escape_before_non_key(
                    self.vim_state,
                    self.textarea,
                    self.state,
                    self.config,
                );
                self.vim_state.cancel_pending_command();
                handle_scroll_lines(self.state, lines, now);
                Ok(None)
            }
            BatchedInputEvent::Event(event) => {
                if consume_focus_event(&event, self.presentation) {
                    return Ok(None);
                }
                if resolve_pending_insert_escape_before_routing(
                    &event,
                    now,
                    self.vim_state,
                    self.textarea,
                    self.state,
                    self.config,
                    self.theme,
                ) == PendingInsertEscapeRouting::Consumed
                {
                    return Ok(None);
                }
                if matches!(event, Event::Paste(_)) {
                    flush_pending_insert_escape_before_non_key(
                        self.vim_state,
                        self.textarea,
                        self.state,
                        self.config,
                    );
                }
                if handle_paste_event(
                    &event,
                    self.state,
                    self.config,
                    self.action_tx,
                    self.textarea,
                ) {
                    self.vim_state.cancel_pending_command();
                    return Ok(None);
                }
                if handle_resize_event(&event, self.state) {
                    return Ok(None);
                }
                if matches!(event, Event::Mouse(_)) {
                    flush_pending_insert_escape_before_non_key(
                        self.vim_state,
                        self.textarea,
                        self.state,
                        self.config,
                    );
                }
                match handle_mouse_event(&event, self.state, self.textarea, now) {
                    MouseFlow::NotMouse => {}
                    MouseFlow::Handled => {
                        self.vim_state.cancel_pending_command();
                        return Ok(None);
                    }
                    MouseFlow::SyntheticEnter => {
                        self.vim_state.cancel_pending_command();
                        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                        let event = Event::Key(key);
                        return self.route_status_key(&event, &key, &mut clear_terminal);
                    }
                }
                let Event::Key(key) = &event else {
                    return Ok(None);
                };
                match handle_key_event_preflight(
                    *key,
                    self.state,
                    self.config,
                    self.action_tx,
                    self.vim_state,
                    !self.textarea.is_empty(),
                    || clear_terminal(),
                )? {
                    KeyEventFlow::Continue => return Ok(None),
                    KeyEventFlow::Exit(code) => return Ok(Some(code)),
                    KeyEventFlow::Unhandled => {}
                }
                self.route_status_key(&event, key, &mut clear_terminal)
            }
        }
    }

    fn route_status_key(
        &mut self,
        event: &Event,
        key: &KeyEvent,
        clear_terminal: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<i32>> {
        match handle_status_key(
            event,
            key,
            self.state,
            self.config,
            self.shared_config,
            self.action_tx,
            self.preloaded_transcript,
            self.textarea,
            self.vim_state,
            self.theme,
            self.initial_prompt.clone(),
            clear_terminal,
        )? {
            StatusKeyFlow::Continue => Ok(None),
            StatusKeyFlow::Exit(code) => Ok(Some(code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crossbeam_channel as mpsc;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use tui_textarea::{Input, Key, TextArea};

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{RunConfig, ThemeName, VimInsertEscapeSequence};
    use orca_runtime::history::SessionTranscript;

    use super::RendererInputRouter;
    use crate::composer_textarea::textarea_text;
    use crate::input_event_actions::BatchedInputEvent;
    use crate::selection::{SelectionGranularity, SelectionPos, TranscriptSelection};
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::types::{AppState, PlanApprovalDialog, UserAction};
    use crate::vim::{VimMode, VimState};

    struct Fixture {
        state: AppState,
        config: RunConfig,
        shared_config: Arc<Mutex<RunConfig>>,
        action_tx: mpsc::Sender<UserAction>,
        action_rx: mpsc::Receiver<UserAction>,
        preloaded: Arc<Mutex<Option<SessionTranscript>>>,
        textarea: TextArea<'static>,
        vim: VimState,
        theme: Theme,
        presentation: TerminalPresentation,
        initial_prompt: Option<String>,
    }

    impl Fixture {
        fn new() -> Self {
            let config = test_run_config();
            let shared_config = Arc::new(Mutex::new(config.clone()));
            let (action_tx, action_rx) = mpsc::unbounded();
            Self {
                state: AppState::new(
                    action_tx.clone(),
                    "test".to_string(),
                    "mock".to_string(),
                    "/tmp".to_string(),
                ),
                config,
                shared_config,
                action_tx,
                action_rx,
                preloaded: Arc::new(Mutex::new(None)),
                textarea: TextArea::default(),
                vim: VimState::new(false),
                theme: Theme::named(ThemeName::Dark),
                presentation: TerminalPresentation::new(
                    true,
                    TerminalPresentationProfile {
                        osc9_supported: true,
                        tmux_passthrough: false,
                    },
                ),
                initial_prompt: None,
            }
        }

        fn route(
            &mut self,
            input: BatchedInputEvent,
            now: Instant,
            clear_terminal: impl FnMut() -> io::Result<()>,
        ) -> io::Result<Option<i32>> {
            RendererInputRouter::new(
                &mut self.state,
                &mut self.config,
                &self.shared_config,
                &self.action_tx,
                &self.preloaded,
                &mut self.textarea,
                &mut self.vim,
                &self.theme,
                &mut self.presentation,
                &self.initial_prompt,
            )
            .route(input, now, clear_terminal)
        }

        fn seed_pending_insert_escape(&mut self, now: Instant) {
            let sequence = VimInsertEscapeSequence::parse("jj").expect("valid sequence");
            self.config.vim_mode = true;
            self.config.vim_insert_escape = Some(sequence.clone());
            *self.shared_config.lock().expect("shared config") = self.config.clone();
            self.vim = VimState::with_insert_escape(true, Some(sequence));
            self.vim.mode = VimMode::Insert;
            self.vim.handle_at(
                Input {
                    key: Key::Char('j'),
                    ctrl: false,
                    alt: false,
                    shift: false,
                },
                &mut self.textarea,
                &self.theme,
                now,
            );
            self.vim.seed_pending_count_for_test();
        }
    }

    fn mouse_down(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn focus_is_consumed_before_other_semantic_routing() {
        let mut fixture = Fixture::new();
        fixture.seed_pending_insert_escape(Instant::now());
        fixture.presentation.set_focused(true);

        let exit = fixture
            .route(
                BatchedInputEvent::Event(Event::FocusLost),
                Instant::now(),
                || panic!("focus must not clear the terminal"),
            )
            .expect("focus routing");

        assert_eq!(exit, None);
        assert!(!fixture.presentation.is_focused());
        assert!(fixture.textarea.is_empty());
        assert!(fixture.vim.has_pending_command_for_test());
        assert!(fixture.action_rx.try_recv().is_err());
    }

    #[test]
    fn scroll_flushes_insert_escape_and_cancels_pending_vim_command_first() {
        let mut fixture = Fixture::new();
        let now = Instant::now();
        fixture.seed_pending_insert_escape(now);

        fixture
            .route(BatchedInputEvent::ScrollLines(-3), now, || Ok(()))
            .expect("scroll routing");

        assert_eq!(textarea_text(&fixture.textarea), "j");
        assert!(!fixture.vim.has_pending_command_for_test());
    }

    #[test]
    fn config_dialog_consumes_coalesced_scroll_without_moving_transcript() {
        let mut fixture = Fixture::new();
        fixture.state.total_lines = 100;
        fixture.state.visible_height = 20;
        fixture.state.scroll_offset = 40;
        fixture.state.config_dialog = Some(crate::types::ConfigDialog {
            selected: 0,
            model: fixture.state.model_name.clone(),
            reasoning_effort: fixture.state.reasoning_effort,
            approval_mode: fixture.state.approval_mode,
        });

        fixture
            .route(
                BatchedInputEvent::ScrollLines(-3),
                Instant::now(),
                || Ok(()),
            )
            .expect("config scroll routing");

        assert_eq!(fixture.state.scroll_offset, 40);
        assert!(fixture.state.config_dialog.is_some());
    }

    #[test]
    fn paste_flushes_insert_escape_before_paste_ownership() {
        let mut fixture = Fixture::new();
        let now = Instant::now();
        fixture.seed_pending_insert_escape(now);

        fixture
            .route(
                BatchedInputEvent::Event(Event::Paste("xy".to_string())),
                now,
                || Ok(()),
            )
            .expect("paste routing");

        assert_eq!(textarea_text(&fixture.textarea), "jxy");
        assert!(!fixture.vim.has_pending_command_for_test());
    }

    #[test]
    fn resize_invalidates_selection_without_key_fallthrough() {
        let mut fixture = Fixture::new();
        let pos = SelectionPos { row: 0, col: 0 };
        fixture.state.selection = Some(TranscriptSelection::unit(
            SelectionGranularity::Cell,
            pos,
            pos,
        ));

        fixture
            .route(
                BatchedInputEvent::Event(Event::Resize(120, 40)),
                Instant::now(),
                || panic!("resize must not clear the terminal"),
            )
            .expect("resize routing");

        assert!(fixture.state.selection.is_none());
        assert!(fixture.action_rx.try_recv().is_err());
    }

    #[test]
    fn mouse_confirmation_dispatches_the_selected_plan_action() {
        let mut fixture = Fixture::new();
        fixture.state.frame_area = Some(Rect::new(0, 0, 100, 30));
        fixture.state.approval_mode = ApprovalMode::Plan;
        fixture.state.pre_plan_approval_mode = Some(ApprovalMode::FullAuto);
        fixture.state.plan_approval_dialog = Some(PlanApprovalDialog {
            plan: "- inspect\n- implement".to_string(),
            selected: 0,
        });
        let option_row = (0..30)
            .find(|row| {
                crate::ui::plan_approval_option_hit_index(&fixture.state, 50, *row) == Some(0)
            })
            .expect("first plan option must be hittable");

        fixture
            .route(
                BatchedInputEvent::Event(mouse_down(50, option_row)),
                Instant::now(),
                || Ok(()),
            )
            .expect("mouse confirmation routing");

        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::ImplementApprovedPlan {
                prompt,
                approval_mode: ApprovalMode::FullAuto,
            }) if prompt == crate::plan_approval_actions::IMPLEMENT_APPROVED_PLAN_PROMPT
        ));
        assert!(fixture.state.plan_approval_dialog.is_none());
    }

    #[test]
    fn real_escape_runs_selection_preflight_before_running_status() {
        let mut fixture = Fixture::new();
        fixture.state.enter_running();
        let pos = SelectionPos { row: 0, col: 0 };
        fixture.state.selection = Some(TranscriptSelection::unit(
            SelectionGranularity::Cell,
            pos,
            pos,
        ));

        fixture
            .route(
                BatchedInputEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                ))),
                Instant::now(),
                || Ok(()),
            )
            .expect("escape routing");

        assert!(fixture.state.selection.is_none());
        assert!(fixture.action_rx.try_recv().is_err());
    }

    #[test]
    fn clear_terminal_error_is_returned_without_translation() {
        let mut fixture = Fixture::new();

        let error = fixture
            .route(
                BatchedInputEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('l'),
                    KeyModifiers::CONTROL,
                ))),
                Instant::now(),
                || Err(io::Error::other("clear failed")),
            )
            .expect_err("clear failure must escape routing");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "clear failed");
    }
}
