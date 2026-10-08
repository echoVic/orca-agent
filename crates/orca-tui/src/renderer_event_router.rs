use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel as mpsc;
use ratatui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::bridge;
use crate::frame_scheduler::IterationEvent;
use crate::global_actions::quit_on_termination_signal;
use crate::input_event_actions::BatchedInputEvent;
use crate::protocol::{TuiEvent, UserAction};
use crate::renderer_input_router::RendererInputRouter;
use crate::renderer_runtime::RendererRuntimeEventOwner;
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::AppState;
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
            IterationEvent::Runtime(TuiEvent::BackendExited(reason)) => {
                self.state.exit_message = Some(reason);
                Ok(Some(1))
            }
            IterationEvent::Runtime(TuiEvent::TerminationSignal { signal }) => Ok(Some(
                quit_on_termination_signal(signal, self.state, self.action_tx),
            )),
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
    use ratatui_textarea::TextArea;

    use orca_core::config::ThemeName;
    use orca_runtime::history::SessionTranscript;
    use orca_runtime::termination_signals::TerminationSignal;

    use super::RendererIterationEventRouter;
    use crate::bridge;
    use crate::frame_scheduler::IterationEvent;
    use crate::input_event_actions::BatchedInputEvent;
    use crate::mention_search_manager::MentionSearchManager;
    use crate::protocol::{TuiEvent, UserAction};
    use crate::renderer_runtime::RendererRuntimeEventOwner;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::types::{AppState, AppStatus};
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
            fixture.state.transcript.messages.as_slice(),
            [ChatMessage::System { text: message, .. }] if message == "routed notice"
        ));
        assert!(fixture.action_rx.try_recv().is_err());
    }

    #[test]
    fn an_attachment_that_ended_for_good_exits_with_its_reason() {
        // Once `orca attach` gave up reconnecting, the TUI stayed open with
        // nothing behind it: a sent message spun as "running" forever.
        let mut fixture = Fixture::new();

        let exit = fixture
            .route(
                IterationEvent::Runtime(TuiEvent::BackendExited(
                    "ACP reconnect limit reached".to_string(),
                )),
                Instant::now(),
                || panic!("an exit does not clear the terminal"),
            )
            .expect("runtime event routing");

        assert_eq!(exit, Some(1));
        assert_eq!(
            fixture.state.exit_message.as_deref(),
            Some("ACP reconnect limit reached")
        );
    }

    /// SIGTERM, SIGHUP or SIGINT quits as an ordinary exit does: a running
    /// turn is interrupted first, and the exit code is 128 plus the signal.
    #[test]
    fn a_termination_signal_interrupts_the_turn_and_quits_with_its_code() {
        let mut fixture = Fixture::new();
        fixture.state.set_status(AppStatus::Running);

        let exit = fixture
            .route(
                IterationEvent::Runtime(TuiEvent::TerminationSignal {
                    signal: TerminationSignal::Terminate,
                }),
                Instant::now(),
                || panic!("a signal does not clear the terminal"),
            )
            .expect("runtime event routing");

        assert_eq!(exit, Some(143));
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Interrupt)
        ));
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
        assert!(!fixture.state.terminal_lost);
    }

    /// After SIGHUP the terminal is gone: nothing more is drawn on it, and
    /// no resume hint is printed to it.
    #[test]
    fn a_hangup_quits_with_its_terminal_lost() {
        let mut fixture = Fixture::new();

        let exit = fixture
            .route(
                IterationEvent::Runtime(TuiEvent::TerminationSignal {
                    signal: TerminationSignal::Hangup,
                }),
                Instant::now(),
                || panic!("a signal does not clear the terminal"),
            )
            .expect("runtime event routing");

        assert_eq!(exit, Some(129));
        assert!(fixture.state.terminal_lost);
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
        assert!(
            fixture.action_rx.try_recv().is_err(),
            "nothing ran to interrupt"
        );
    }

    /// The turn of a session attached with `orca attach` is the daemon's:
    /// quitting only disconnects from it.
    #[test]
    fn an_attached_tui_quits_without_interrupting_the_daemons_turn() {
        let mut fixture = Fixture::new();
        fixture.state.attached_session = true;
        fixture.state.set_status(AppStatus::Running);

        let exit = fixture
            .route(
                IterationEvent::Runtime(TuiEvent::TerminationSignal {
                    signal: TerminationSignal::Interrupt,
                }),
                Instant::now(),
                || panic!("a signal does not clear the terminal"),
            )
            .expect("runtime event routing");

        assert_eq!(exit, Some(130));
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
        assert!(
            fixture.action_rx.try_recv().is_err(),
            "the daemon's turn was interrupted"
        );
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
