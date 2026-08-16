use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel as mpsc;
use ratatui::Terminal;
use ratatui::backend::Backend;
use tui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::bridge;
use crate::input_event_actions::coalesce_input_events;
use crate::insert_escape::flush_expired_insert_escape;
use crate::renderer_event_router::RendererIterationEventRouter;
use crate::renderer_frame::RendererFrameOwner;
use crate::renderer_input_wake::RendererInputWakeOwner;
use crate::renderer_interaction_acks::RendererInteractionAckOwner;
use crate::renderer_runtime::RendererRuntimeEventOwner;
use crate::renderer_runtime_inbox::RendererRuntimeInboxOwner;
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, UserAction};
use crate::vim::VimState;

pub(crate) struct RendererLoopOwner<'a, 'text> {
    frame: RendererFrameOwner,
    max_runtime_events: usize,
    input_wake: &'a RendererInputWakeOwner,
    interaction_acks: &'a RendererInteractionAckOwner,
    runtime_inbox: &'a RendererRuntimeInboxOwner,
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
    workspace_root: &'a Path,
}

impl<'a, 'text> RendererLoopOwner<'a, 'text> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        initial_draw_at: Instant,
        frame_interval: Duration,
        animation_interval: Duration,
        max_runtime_events: usize,
        input_wake: &'a RendererInputWakeOwner,
        interaction_acks: &'a RendererInteractionAckOwner,
        runtime_inbox: &'a RendererRuntimeInboxOwner,
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
        workspace_root: &'a Path,
    ) -> Self {
        Self {
            frame: RendererFrameOwner::new(initial_draw_at, frame_interval, animation_interval),
            max_runtime_events,
            input_wake,
            interaction_acks,
            runtime_inbox,
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
            workspace_root,
        }
    }

    pub(crate) fn run<B, ClearTerminal, CopyClipboard, WritePending>(
        self,
        terminal: &mut Terminal<B>,
        mut clear_terminal: ClearTerminal,
        mut copy_clipboard: CopyClipboard,
        mut write_pending: WritePending,
    ) -> io::Result<i32>
    where
        B: Backend,
        ClearTerminal: FnMut(&mut Terminal<B>) -> io::Result<()>,
        CopyClipboard: FnMut(&str),
        WritePending: FnMut(&mut Terminal<B>, &mut TerminalPresentation, AppStatus),
    {
        let Self {
            mut frame,
            max_runtime_events,
            input_wake,
            interaction_acks,
            runtime_inbox,
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
            workspace_root,
        } = self;

        loop {
            let now = Instant::now();
            if flush_expired_insert_escape(now, vim_state, textarea, state, config) {
                frame.mark_dirty();
            }
            let poll_timeout = frame.prepare_iteration(now, state, presentation);

            let input_events =
                input_wake.receive(poll_timeout, || frame.resume(terminal, presentation))?;

            if interaction_acks.drain(state, textarea, vim_state, theme) {
                frame.mark_dirty();
            }

            let iteration = frame.run_iteration(
                coalesce_input_events(input_events, 3),
                runtime_inbox.pending(),
                usize::MAX,
                max_runtime_events,
                Instant::now,
                |event| {
                    RendererIterationEventRouter::new(
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
                    )
                    .route(event, Instant::now(), || clear_terminal(terminal))
                },
            )?;
            runtime.sync_composer(config, workspace_root, state, textarea, Instant::now());
            if let Some(code) = iteration.exit_code {
                return Ok(code);
            }
            frame.present_iteration(
                terminal,
                presentation,
                state,
                textarea,
                theme,
                iteration.draw_at,
                |text| copy_clipboard(text),
                |terminal, presentation, status| {
                    write_pending(terminal, presentation, status);
                },
            )?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crossbeam_channel as mpsc;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tui_textarea::TextArea;

    use orca_core::config::ThemeName;
    use orca_runtime::history::SessionTranscript;

    use super::RendererLoopOwner;
    use crate::bridge;
    use crate::channels::{TUI_EVENT_CAPACITY, TuiEventSender, tui_event_channel};
    use crate::input_runtime::InputControl;
    use crate::mention_search_manager::MentionSearchManager;
    use crate::renderer_input_wake::RendererInputWakeOwner;
    use crate::renderer_interaction_acks::RendererInteractionAckOwner;
    use crate::renderer_runtime::RendererRuntimeEventOwner;
    use crate::renderer_runtime_inbox::RendererRuntimeInboxOwner;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::terminal_session::TerminalInputReceivers;
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::types::{AppState, ChatMessage, TuiEvent, UserAction};
    use crate::vim::VimState;

    struct Fixture {
        _root: tempfile::TempDir,
        workspace_root: PathBuf,
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
        input_wake: RendererInputWakeOwner,
        input_tx: mpsc::Sender<Event>,
        _focus_tx: mpsc::Sender<Event>,
        control_tx: mpsc::Sender<InputControl>,
        runtime_tx: TuiEventSender,
        runtime_inbox: RendererRuntimeInboxOwner,
        interaction_acks: RendererInteractionAckOwner,
        runtime: RendererRuntimeEventOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("renderer loop root");
            let workspace_root = root.path().to_path_buf();
            let mut config = test_run_config();
            config.cwd = Some(workspace_root.clone());
            let shared_config = Arc::new(Mutex::new(config.clone()));
            let (action_tx, action_rx) = mpsc::unbounded();
            let (input_tx, input_rx) = mpsc::bounded(8);
            let (focus_tx, focus_rx) = mpsc::bounded(8);
            let (control_tx, control_rx) = mpsc::bounded(8);
            let input_wake = RendererInputWakeOwner::new(
                TerminalInputReceivers::from_parts_for_test(input_rx, focus_rx, control_rx),
                64,
            );
            let (runtime_tx, runtime_rx) = tui_event_channel();
            let runtime_inbox = RendererRuntimeInboxOwner::new(runtime_rx);
            let (_ack_tx, ack_rx) = mpsc::unbounded();
            let interaction_acks = RendererInteractionAckOwner::new(ack_rx);
            let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
            let runtime = RendererRuntimeEventOwner::new(
                MentionSearchManager::new(workspace_root.clone(), mention_event_tx),
                None,
            );
            Self {
                state: AppState::new(
                    action_tx.clone(),
                    "test".to_string(),
                    "mock".to_string(),
                    workspace_root.display().to_string(),
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
                input_wake,
                input_tx,
                _focus_tx: focus_tx,
                control_tx,
                runtime_tx,
                runtime_inbox,
                interaction_acks,
                runtime,
                workspace_root,
                _root: root,
            }
        }

        fn queue_key(&self, code: KeyCode, modifiers: KeyModifiers) {
            self.input_tx
                .send(Event::Key(KeyEvent::new(code, modifiers)))
                .expect("input receiver alive");
        }

        fn run<ClearTerminal, CopyClipboard, WritePending>(
            &mut self,
            terminal: &mut Terminal<TestBackend>,
            clear_terminal: ClearTerminal,
            copy_clipboard: CopyClipboard,
            write_pending: WritePending,
        ) -> io::Result<i32>
        where
            ClearTerminal: FnMut(&mut Terminal<TestBackend>) -> io::Result<()>,
            CopyClipboard: FnMut(&str),
            WritePending: FnMut(
                &mut Terminal<TestBackend>,
                &mut TerminalPresentation,
                crate::types::AppStatus,
            ),
        {
            RendererLoopOwner::new(
                Instant::now(),
                Duration::ZERO,
                Duration::from_millis(80),
                TUI_EVENT_CAPACITY,
                &self.input_wake,
                &self.interaction_acks,
                &self.runtime_inbox,
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
                &self.workspace_root,
            )
            .run(terminal, clear_terminal, copy_clipboard, write_pending)
        }
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).expect("test terminal")
    }

    #[test]
    fn input_exit_syncs_then_skips_presentation() {
        let mut fixture = Fixture::new();
        fixture.state.last_ctrl_c = Some(Instant::now());
        fixture.queue_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut terminal = terminal();

        let exit = fixture
            .run(
                &mut terminal,
                |_| Ok(()),
                |_| panic!("exit must not consume clipboard output"),
                |_, _, _| panic!("exit must precede pending presentation"),
            )
            .expect("renderer exit");

        assert_eq!(exit, 130);
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
    }

    #[test]
    fn runtime_event_presents_once_before_the_next_input_exit() {
        let mut fixture = Fixture::new();
        fixture.state.last_ctrl_c = Some(Instant::now());
        fixture
            .control_tx
            .send(InputControl::Resumed)
            .expect("control receiver alive");
        fixture.queue_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        fixture
            .runtime_tx
            .send(TuiEvent::Notice("loop notice".to_string()))
            .expect("runtime receiver alive");
        let presentations = Rc::new(Cell::new(0));
        let presentation_count = Rc::clone(&presentations);
        let mut terminal = terminal();

        let exit = fixture
            .run(
                &mut terminal,
                |_| Ok(()),
                |_| panic!("notice path has no clipboard output"),
                move |_, _, _| presentation_count.set(presentation_count.get() + 1),
            )
            .expect("runtime then exit");

        assert_eq!(exit, 130);
        assert_eq!(presentations.get(), 1);
        assert!(matches!(
            fixture.state.messages.as_slice(),
            [ChatMessage::System(message)] if message == "loop notice"
        ));
        assert!(matches!(
            fixture.action_rx.try_recv(),
            Ok(UserAction::Cancel)
        ));
    }

    #[test]
    fn input_clear_error_propagates_without_presentation() {
        let mut fixture = Fixture::new();
        fixture.queue_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let mut terminal = terminal();

        let error = fixture
            .run(
                &mut terminal,
                |_| Err(io::Error::other("exact loop clear failure")),
                |_| panic!("failed input must not consume clipboard output"),
                |_, _, _| panic!("failed input must not reach presentation"),
            )
            .expect_err("clear error must escape the loop");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "exact loop clear failure");
    }
}
