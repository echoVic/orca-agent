use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel as mpsc;
use tui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::bridge;
use crate::frame_scheduler::IterationEvent;
use crate::input_event_actions::BatchedInputEvent;
use crate::renderer_input_router::RendererInputRouter;
use crate::renderer_runtime::RendererRuntimeEventOwner;
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::{AppState, TuiEvent, UserAction};
use crate::vim::VimState;

pub(crate) struct RendererIterationEventRouter<'a, 'text> {
    runtime: &'a mut RendererRuntimeEventOwner,
    state: &'a mut AppState,
    config: &'a mut RunConfig,
    shared_config: &'a Arc<Mutex<RunConfig>>,
    action_tx: &'a mpsc::Sender<UserAction>,
    pending_workflow_notifications: &'a bridge::PendingWorkflowNotifications,
    preloaded_transcript: &'a Arc<Mutex<Option<SessionTranscript>>>,
    textarea: &'a mut TextArea<'text>,
    vim_state: &'a mut VimState,
    theme: &'a Theme,
    presentation: &'a mut TerminalPresentation,
    initial_prompt: &'a Option<String>,
}

impl<'a, 'text> RendererIterationEventRouter<'a, 'text> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        runtime: &'a mut RendererRuntimeEventOwner,
        state: &'a mut AppState,
        config: &'a mut RunConfig,
        shared_config: &'a Arc<Mutex<RunConfig>>,
        action_tx: &'a mpsc::Sender<UserAction>,
        pending_workflow_notifications: &'a bridge::PendingWorkflowNotifications,
        preloaded_transcript: &'a Arc<Mutex<Option<SessionTranscript>>>,
        textarea: &'a mut TextArea<'text>,
        vim_state: &'a mut VimState,
        theme: &'a Theme,
        presentation: &'a mut TerminalPresentation,
        initial_prompt: &'a Option<String>,
    ) -> Self {
        Self {
            runtime,
            state,
            config,
            shared_config,
            action_tx,
            pending_workflow_notifications,
            preloaded_transcript,
            textarea,
            vim_state,
            theme,
            presentation,
            initial_prompt,
        }
    }

    pub(crate) fn route(
        self,
        event: IterationEvent<BatchedInputEvent, TuiEvent>,
        now: Instant,
        clear_terminal: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<i32>> {
        match event {
            IterationEvent::Input(input) => RendererInputRouter::new(
                self.state,
                self.config,
                self.shared_config,
                self.action_tx,
                self.preloaded_transcript,
                self.textarea,
                self.vim_state,
                self.theme,
                self.presentation,
                self.initial_prompt,
            )
            .route(input, now, clear_terminal),
            IterationEvent::Runtime(tui_event) => {
                self.runtime.handle(
                    tui_event,
                    self.state,
                    self.config,
                    self.action_tx,
                    self.pending_workflow_notifications,
                    self.textarea,
                    self.vim_state,
                    self.theme,
                    self.presentation,
                );
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crossbeam_channel as mpsc;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tui_textarea::TextArea;

    use orca_core::config::ThemeName;
    use orca_runtime::history::SessionTranscript;

    use super::RendererIterationEventRouter;
    use crate::bridge;
    use crate::frame_scheduler::IterationEvent;
    use crate::input_event_actions::BatchedInputEvent;
    use crate::mention_search_manager::MentionSearchManager;
    use crate::renderer_runtime::RendererRuntimeEventOwner;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::types::{AppState, ChatMessage, TuiEvent, UserAction};
    use crate::vim::VimState;

    struct Fixture {
        _root: tempfile::TempDir,
        state: AppState,
        config: orca_core::config::RunConfig,
        shared_config: Arc<Mutex<orca_core::config::RunConfig>>,
        action_tx: mpsc::Sender<UserAction>,
        action_rx: mpsc::Receiver<UserAction>,
        pending_workflow_notifications: bridge::PendingWorkflowNotifications,
        preloaded: Arc<Mutex<Option<SessionTranscript>>>,
        textarea: TextArea<'static>,
        vim: VimState,
        theme: Theme,
        presentation: TerminalPresentation,
        initial_prompt: Option<String>,
        runtime: RendererRuntimeEventOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("event router root");
            let config = test_run_config();
            let shared_config = Arc::new(Mutex::new(config.clone()));
            let (action_tx, action_rx) = mpsc::unbounded();
            let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
            let runtime = RendererRuntimeEventOwner::new(
                MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx),
                None,
            );
            Self {
                state: AppState::new(
                    action_tx.clone(),
                    "test".to_string(),
                    "mock".to_string(),
                    root.path().display().to_string(),
                ),
                config,
                shared_config,
                action_tx,
                action_rx,
                pending_workflow_notifications: bridge::PendingWorkflowNotifications::new(),
                preloaded: Arc::new(Mutex::new(None)),
                textarea: TextArea::default(),
                vim: VimState::new(false),
                theme: Theme::named(ThemeName::Dark),
                presentation: TerminalPresentation::new(
                    false,
                    TerminalPresentationProfile {
                        osc9_supported: false,
                        tmux_passthrough: false,
                    },
                ),
                initial_prompt: None,
                runtime,
                _root: root,
            }
        }

        fn route(
            &mut self,
            event: IterationEvent<BatchedInputEvent, TuiEvent>,
            now: Instant,
            clear_terminal: impl FnMut() -> io::Result<()>,
        ) -> io::Result<Option<i32>> {
            RendererIterationEventRouter::new(
                &mut self.runtime,
                &mut self.state,
                &mut self.config,
                &self.shared_config,
                &self.action_tx,
                &self.pending_workflow_notifications,
                &self.preloaded,
                &mut self.textarea,
                &mut self.vim,
                &self.theme,
                &mut self.presentation,
                &self.initial_prompt,
            )
            .route(event, now, clear_terminal)
        }
    }

    #[test]
    fn runtime_event_delegates_once_and_never_fabricates_an_exit() {
        let mut fixture = Fixture::new();

        let exit = fixture
            .route(
                IterationEvent::Runtime(TuiEvent::Notice("routed notice".to_string())),
                Instant::now(),
                || panic!("runtime events must not clear the terminal"),
            )
            .expect("runtime event routing");

        assert_eq!(exit, None);
        assert!(matches!(
            fixture.state.messages.as_slice(),
            [ChatMessage::System(message)] if message == "routed notice"
        ));
        assert!(fixture.action_rx.try_recv().is_err());
    }

    #[test]
    fn input_exit_code_and_cancel_action_propagate_exactly() {
        let mut fixture = Fixture::new();
        fixture.state.last_ctrl_c = Some(Instant::now());

        let exit = fixture
            .route(
                IterationEvent::Input(BatchedInputEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )))),
                Instant::now(),
                || Ok(()),
            )
            .expect("input exit routing");

        assert_eq!(exit, Some(130));
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
    }

    #[test]
    fn input_terminal_error_propagates_without_translation() {
        let mut fixture = Fixture::new();

        let error = fixture
            .route(
                IterationEvent::Input(BatchedInputEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('l'),
                    KeyModifiers::CONTROL,
                )))),
                Instant::now(),
                || Err(io::Error::other("exact clear failure")),
            )
            .expect_err("input clear error must escape the router");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "exact clear failure");
    }
}
