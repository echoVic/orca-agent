use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use tui_textarea::TextArea;

use crate::attachment_routing::accept_attached_tui_event;
use crate::bridge;
use crate::composer_images::DeferredImageSubmit;
use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::{textarea_cursor_byte_index, textarea_text};
use crate::idle_submit_actions::{
    continue_held_submissions, handle_idle_submit, release_held_submissions,
    start_turn_after_history,
};
use crate::mention_search_manager::MentionSearchManager;
use crate::protocol::{TuiEvent, UserAction};
use crate::queued_input_actions::enqueue_composer_follow_up_to_runtime;
use crate::runtime_event_actions::handle_runtime_event;
use crate::surface_actions::TuiSurfaceActions;
use crate::surface_projection::{McpServerStatusView, McpServerView};
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::transcript_state::ChatMessage;
use crate::types::AppState;
use crate::vim::VimState;
use crate::workspace_config::mention_search_roots;

pub(crate) struct RendererRuntimeEventOwner {
    mention_search: MentionSearchManager,
    pending_initial_prompt: Option<String>,
    local_shell_readiness: bool,
    /// The MCP servers in view when the mention catalog last looked at
    /// them.
    mcp_servers_seen: Vec<McpServerView>,
}

impl RendererRuntimeEventOwner {
    pub(crate) fn new(
        mention_search: MentionSearchManager,
        pending_initial_prompt: Option<String>,
    ) -> Self {
        Self {
            mention_search,
            pending_initial_prompt,
            local_shell_readiness: true,
            mcp_servers_seen: Vec::new(),
        }
    }

    pub(crate) fn with_local_shell_readiness(mut self, enabled: bool) -> Self {
        self.local_shell_readiness = enabled;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle(
        &mut self,
        tui_event: TuiEvent,
        state: &mut AppState,
        config: &mut RunConfig,
        action_tx: &mpsc::Sender<UserAction>,
        pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
        textarea: &mut TextArea,
        vim_state: &mut VimState,
        theme: &Theme,
        presentation: &mut TerminalPresentation,
    ) {
        if let TuiEvent::ClipboardImagePasteCompleted { request_id, result } = tui_event {
            self.handle_clipboard_image_paste(
                request_id, result, state, config, action_tx, textarea, vim_state, theme,
            );
            return;
        }
        let tui_event = match accept_attached_tui_event(state, tui_event) {
            Ok(Some(tui_event)) => tui_event,
            Ok(None) | Err(()) => return,
        };
        match tui_event {
            // The wait for the history of the conversation resumed at launch
            // ends when it loads, and when a new conversation takes its place
            // (`/new`): there is no history to wait for then, and none will
            // come for the prompt given with it, whose turn is the new
            // conversation's first.
            TuiEvent::HistoryLoaded { .. } | TuiEvent::NewSessionStarted => {
                let new_conversation = matches!(tui_event, TuiEvent::NewSessionStarted);
                let wait_ends = !new_conversation || state.startup_history_pending;
                if new_conversation {
                    config.history_mode = HistoryMode::Record;
                }
                handle_runtime_event(
                    tui_event,
                    state,
                    action_tx,
                    pending_workflow_notifications,
                    textarea,
                    vim_state,
                    theme,
                    presentation,
                );
                // The prompt given on the command line is sent now, and what the
                // user sent meanwhile comes after its turn: once that turn's
                // operation is active, not before, or it would wait behind the
                // whole turn in the controller's own line, where nothing shows
                // it (`TuiEvent::OperationActive`).
                if wait_ends {
                    start_turn_after_history(state, action_tx, self.pending_initial_prompt.take());
                }
            }
            TuiEvent::OperationActive { token } => {
                release_held_submissions(state, action_tx, token);
            }
            TuiEvent::TurnNotStarted { token } => {
                continue_held_submissions(state, action_tx, token);
            }
            TuiEvent::MentionSearchDirty { generation } => {
                let text = textarea_text(textarea);
                let cursor = textarea_cursor_byte_index(textarea);
                self.mention_search
                    .consume_dirty_at_cursor(generation, &text, cursor, state);
            }
            TuiEvent::MentionCatalogDirty { generation } => {
                self.mention_search.consume_catalog_dirty(generation, state);
            }
            TuiEvent::MentionRuntimeReady(thread) => {
                self.mention_search
                    .install_runtime_actions(TuiSurfaceActions::new(thread));
            }
            TuiEvent::SettingsUpdated {
                model,
                reasoning_effort,
                approval_mode,
            } => {
                config.model = config.model.with_value_unchecked(Some(model.clone()));
                config.reasoning_effort = reasoning_effort;
                config.approval_mode = approval_mode;
                config.execution_profile =
                    orca_core::capability::ExecutionProfile::for_approval_mode(approval_mode);
                if approval_mode == orca_core::approval_types::ApprovalMode::FullAuto {
                    config.active_permission_profile = None;
                }
                handle_runtime_event(
                    TuiEvent::SettingsUpdated {
                        model,
                        reasoning_effort,
                        approval_mode,
                    },
                    state,
                    action_tx,
                    pending_workflow_notifications,
                    textarea,
                    vim_state,
                    theme,
                    presentation,
                );
                if self.local_shell_readiness
                    && let Some(warning) = orca_runtime::shell_readiness_warning(config)
                {
                    handle_runtime_event(
                        TuiEvent::StartupWarning(warning),
                        state,
                        action_tx,
                        pending_workflow_notifications,
                        textarea,
                        vim_state,
                        theme,
                        presentation,
                    );
                }
            }
            tui_event => {
                handle_runtime_event(
                    tui_event,
                    state,
                    action_tx,
                    pending_workflow_notifications,
                    textarea,
                    vim_state,
                    theme,
                    presentation,
                );
            }
        }
        crate::background_approval_actions::continue_allowed_background_approvals(
            state, config, action_tx,
        );
        self.discover_mentions_after_mcp_changes(state);
    }

    /// Has the mention catalog discovered again once the MCP servers in view
    /// have changed and none is still connecting: a server that connected
    /// after it was discovered offers resources it lacks.
    fn discover_mentions_after_mcp_changes(&mut self, state: &AppState) {
        if state.mcp_catalog.servers == self.mcp_servers_seen {
            return;
        }
        self.mcp_servers_seen.clone_from(&state.mcp_catalog.servers);
        if !self
            .mcp_servers_seen
            .iter()
            .any(|server| server.status == McpServerStatusView::Starting)
        {
            self.mention_search.rediscover_catalog();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_clipboard_image_paste(
        &mut self,
        request_id: u64,
        result: Result<Vec<crate::clipboard_image::ClipboardImagePayload>, String>,
        state: &mut AppState,
        config: &mut RunConfig,
        action_tx: &mpsc::Sender<UserAction>,
        textarea: &mut TextArea,
        vim_state: &mut VimState,
        theme: &Theme,
    ) {
        if !state.composer_images.is_current_request(request_id) {
            return;
        }
        let payloads = match result {
            Ok(payloads) => payloads,
            Err(error) => {
                state.composer_images.fail_paste(request_id);
                state.push_message(ChatMessage::Error(error));
                return;
            }
        };
        let visible_text = textarea_text(textarea);
        let cursor = textarea_cursor_byte_index(textarea);
        let previous_images = state.composer_images.clone();
        let (insertion, _count, deferred) =
            match state
                .composer_images
                .complete_paste(request_id, &visible_text, cursor, payloads)
            {
                Ok(completion) => completion,
                Err(error) => {
                    state.push_message(ChatMessage::Error(error));
                    return;
                }
            };
        if !textarea.insert_str(&insertion) {
            state.composer_images = previous_images;
            state.push_message(ChatMessage::Error(
                "failed to insert image attachment into the composer".to_string(),
            ));
            return;
        }
        state.reset_history_navigation();
        refresh_input_menus(textarea, state, config);

        match deferred {
            Some(DeferredImageSubmit::Submit) => {
                let shared = Arc::new(Mutex::new(config.clone()));
                handle_idle_submit(
                    textarea, vim_state, theme, state, config, &shared, action_tx,
                );
            }
            Some(DeferredImageSubmit::Queue) => {
                enqueue_composer_follow_up_to_runtime(state, action_tx, textarea, vim_state, theme);
            }
            None => {}
        }
    }

    pub(crate) fn sync_composer(
        &mut self,
        config: &RunConfig,
        workspace_root: &Path,
        state: &mut AppState,
        textarea: &TextArea,
        now: Instant,
    ) {
        let mention_enabled = MentionSearchManager::is_enabled(state);
        self.mention_search
            .set_roots(mention_search_roots(config, workspace_root), state);
        let text = textarea_text(textarea);
        let cursor = textarea_cursor_byte_index(textarea);
        state.mention_bindings.reconcile(&text);
        state.atomic_skill_tokens.reconcile(&text);
        state.composer_images.reconcile(&text);
        self.mention_search
            .sync_at_cursor(&text, cursor, mention_enabled, state, now);
    }

    pub(crate) fn shutdown(&mut self) {
        self.mention_search.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel as mpsc;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{ReasoningEffort, RunConfig, ThemeName};
    use orca_runtime::prompt_queue::{
        ClientUserMessageId, PromptQueueInput, PromptQueueSnapshot, QueuedSubmission,
        QueuedSubmissionId,
    };
    use tui_textarea::TextArea;

    use super::RendererRuntimeEventOwner;
    use crate::bridge;
    use crate::composer_images::ComposerImageState;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::idle_submit_actions::handle_idle_submit;
    use crate::mention_search_manager::MentionSearchManager;
    use crate::protocol::SessionAttachmentId;
    use crate::protocol::{AttachedTuiEvent, SubmitToken, TuiEvent, UserAction};
    use crate::queued_input::HeldSubmission;
    use crate::queued_input_actions::handle_running_key;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::types::{AppState, AppStatus};
    use crate::vim::VimState;

    fn attached(attachment: SessionAttachmentId, event: TuiEvent) -> TuiEvent {
        TuiEvent::Attached(Box::new(AttachedTuiEvent {
            attachment: Some(attachment),
            event,
        }))
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

    fn clipboard_payload() -> crate::clipboard_image::ClipboardImagePayload {
        crate::clipboard_image::ClipboardImagePayload {
            media_type: "image/png".to_string(),
            data: b"\x89PNG\r\n\x1a\nfixture".to_vec(),
            width: 2,
            height: 1,
            source_name: None,
        }
    }

    #[test]
    fn clipboard_image_completion_inserts_attachment_without_blocking_input() {
        let root = tempfile::tempdir().expect("temp root");
        let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
        let mut owner = RendererRuntimeEventOwner::new(
            MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx),
            None,
        );
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            root.path().display().to_string(),
        );
        let request_id = state.composer_images.begin_paste().unwrap();
        let mut config = crate::test_support::test_run_config();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea =
            crate::composer_textarea::make_textarea_with_text("describe this", &vim_state, &theme);
        let mut terminal = presentation();

        owner.handle(
            TuiEvent::ClipboardImagePasteCompleted {
                request_id,
                result: Ok(vec![clipboard_payload()]),
            },
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut terminal,
        );

        assert_eq!(
            crate::composer_textarea::textarea_text(&textarea),
            "describe this [Image #1] "
        );
        assert_eq!(
            state
                .composer_images
                .attachments_for_text("describe this [Image #1] ")
                .len(),
            1
        );
        assert!(!state.composer_images.is_paste_in_flight());
    }

    #[test]
    fn enter_while_clipboard_read_is_pending_submits_after_attachment_arrives() {
        let root = tempfile::tempdir().expect("temp root");
        let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
        let mut owner = RendererRuntimeEventOwner::new(
            MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx),
            None,
        );
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            root.path().display().to_string(),
        );
        let request_id = state.composer_images.begin_paste().unwrap();
        state
            .composer_images
            .defer_submit(crate::composer_images::DeferredImageSubmit::Submit);
        let mut config = crate::test_support::test_run_config();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea =
            crate::composer_textarea::make_textarea_with_text("inspect", &vim_state, &theme);
        let mut terminal = presentation();

        owner.handle(
            TuiEvent::ClipboardImagePasteCompleted {
                request_id,
                result: Ok(vec![clipboard_payload()]),
            },
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut terminal,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, images, .. })
                if prompt == "inspect [Image #1]" && images.len() == 1
        ));
        assert!(crate::composer_textarea::textarea_text(&textarea).is_empty());
        assert!(state.composer_images.is_empty());
    }

    #[test]
    fn stale_history_preserves_initial_prompt_and_admitted_history_submits_once() {
        let root = tempfile::tempdir().expect("temp root");
        let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
        let mention_search = MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx);
        let mut owner =
            RendererRuntimeEventOwner::new(mention_search, Some("follow up".to_string()));
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            root.path().display().to_string(),
        );
        let mut config = crate::test_support::test_run_config();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let mut presentation = presentation();
        let stale = SessionAttachmentId::new(1);
        let active = stale.next();

        owner.handle(
            attached(active, TuiEvent::SessionAttachmentActivated),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );
        owner.handle(
            attached(
                stale,
                TuiEvent::HistoryLoaded {
                    messages: vec![ChatMessage::Assistant("stale".to_string())],
                    plan: None,
                    label: "stale history".to_string(),
                },
            ),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert_eq!(owner.pending_initial_prompt.as_deref(), Some("follow up"));
        assert!(action_rx.try_recv().is_err());
        assert!(state.transcript.messages.is_empty());

        owner.handle(
            attached(
                active,
                TuiEvent::HistoryLoaded {
                    messages: vec![ChatMessage::Assistant("hydrated".to_string())],
                    plan: None,
                    label: "loaded history".to_string(),
                },
            ),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert!(owner.pending_initial_prompt.is_none());
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, token: None, .. }) if prompt == "follow up"
        ));
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            state.transcript.messages.as_slice(),
            [
                ChatMessage::Assistant(history),
                ChatMessage::System { text: label, .. },
                ChatMessage::User(prompt),
            ] if history == "hydrated"
                && label == "loaded history"
                && prompt == "follow up"
        ));
        assert_eq!(state.status, AppStatus::Running);

        owner.handle(
            attached(
                active,
                TuiEvent::HistoryLoaded {
                    messages: vec![ChatMessage::Assistant("hydrated again".to_string())],
                    plan: None,
                    label: "loaded again".to_string(),
                },
            ),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::Idle);
        owner.shutdown();
    }

    #[test]
    fn stale_settings_change_nothing_and_admitted_settings_mirror_config_and_state() {
        let root = tempfile::tempdir().expect("temp root");
        let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
        let mention_search = MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx);
        let mut owner = RendererRuntimeEventOwner::new(mention_search, None);
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            root.path().display().to_string(),
        );
        let mut config = crate::test_support::test_run_config();
        let original_model = config.model.display_name().to_string();
        let original_effort = config.reasoning_effort;
        let original_approval = config.approval_mode;
        config.active_permission_profile = Some(orca_core::config::ActivePermissionProfile::new(
            "strict",
            None::<String>,
        ));
        state.reasoning_effort = original_effort;
        state.approval_mode = original_approval;
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let mut presentation = presentation();
        let stale = SessionAttachmentId::new(1);
        let active = stale.next();
        let settings = || TuiEvent::SettingsUpdated {
            model: "deepseek-reasoner".to_string(),
            reasoning_effort: ReasoningEffort::High,
            approval_mode: ApprovalMode::FullAuto,
        };

        owner.handle(
            attached(active, TuiEvent::SessionAttachmentActivated),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );
        owner.handle(
            attached(stale, settings()),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert_eq!(config.model.display_name(), original_model);
        assert_eq!(config.reasoning_effort, original_effort);
        assert_eq!(config.approval_mode, original_approval);
        assert_eq!(state.model_name, "mock");
        assert_eq!(state.reasoning_effort, original_effort);
        assert_eq!(state.approval_mode, original_approval);

        owner.handle(
            attached(active, settings()),
            &mut state,
            &mut config,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert_eq!(config.model.display_name(), "deepseek-reasoner");
        assert_eq!(config.reasoning_effort, ReasoningEffort::High);
        assert_eq!(config.approval_mode, ApprovalMode::FullAuto);
        assert_eq!(
            config.execution_profile,
            orca_core::capability::ExecutionProfile::TrustedHost
        );
        assert!(config.active_permission_profile.is_none());
        assert_eq!(state.model_name, "deepseek-reasoner");
        assert_eq!(state.reasoning_effort, ReasoningEffort::High);
        assert_eq!(state.approval_mode, ApprovalMode::FullAuto);
        owner.shutdown();
    }

    #[test]
    fn the_mention_catalog_is_discovered_again_once_mcp_servers_have_connected() {
        use crate::surface_projection::{
            McpCatalogView, McpServerStatusView, McpServerView, SurfaceProjectionState,
        };

        let home = crate::test_support::isolate_orca_home();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = orca_core::config::HistoryMode::Record;
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "mention catalog")
            .expect("runtime thread");
        let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
        let mut owner = RendererRuntimeEventOwner::new(
            MentionSearchManager::new(home.path().to_path_buf(), mention_event_tx),
            None,
        );
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = TextArea::default();
        let mut terminal = presentation();
        let mut handle = |owner: &mut RendererRuntimeEventOwner, event| {
            owner.handle(
                event,
                &mut state,
                &mut config,
                &action_tx,
                &pending,
                &mut textarea,
                &mut vim_state,
                &theme,
                &mut terminal,
            );
            owner.mention_search.catalog_generation()
        };
        // The thread's projections, as its MCP servers connect.
        let projection = |seq: u64, status: McpServerStatusView| {
            TuiEvent::SurfaceProjectionSynced(Box::new(SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(seq),
                session_id: Some("session-1".to_string()),
                title: "New conversation".to_string(),
                usage_revision: 0,
                usage: Default::default(),
                context_revision: 0,
                context_used_tokens: 0,
                context_limit_tokens: 0,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
                mcp_catalog: McpCatalogView {
                    servers: vec![McpServerView {
                        name: "docs".to_string(),
                        status,
                        prompts_error: None,
                    }],
                    ..McpCatalogView::default()
                },
            }))
        };

        // The runtime is ready: the catalog is discovered.
        assert_eq!(
            handle(
                &mut owner,
                TuiEvent::MentionRuntimeReady(thread.typed_surface())
            ),
            1
        );
        // A server still connecting will list its resources once it is done.
        assert_eq!(
            handle(&mut owner, projection(1, McpServerStatusView::Starting)),
            1
        );
        // It is: the catalog is discovered again, with them.
        assert_eq!(
            handle(&mut owner, projection(2, McpServerStatusView::Connected)),
            2
        );
        // Nothing changed for the servers since.
        assert_eq!(
            handle(&mut owner, projection(3, McpServerStatusView::Connected)),
            2
        );

        owner.shutdown();
        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    }

    /// A TUI waiting for the history of the conversation `--resume` named,
    /// with `prompt` given on the command line, if any.
    struct ResumingTui {
        _root: tempfile::TempDir,
        owner: RendererRuntimeEventOwner,
        state: AppState,
        config: RunConfig,
        action_tx: mpsc::Sender<UserAction>,
        action_rx: mpsc::Receiver<UserAction>,
        pending: bridge::PendingWorkflowNotifications,
        theme: Theme,
        vim_state: VimState,
        textarea: TextArea<'static>,
        presentation: TerminalPresentation,
    }

    impl ResumingTui {
        fn waiting_for_history(prompt: Option<&str>) -> Self {
            let root = tempfile::tempdir().expect("temp root");
            let (mention_event_tx, _mention_event_rx) = mpsc::unbounded();
            let mention_search =
                MentionSearchManager::new(root.path().to_path_buf(), mention_event_tx);
            let owner = RendererRuntimeEventOwner::new(mention_search, prompt.map(str::to_string));
            let (action_tx, action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                root.path().display().to_string(),
            );
            state.startup_history_pending = true;
            let theme = Theme::named(ThemeName::Dark);
            let vim_state = VimState::new(false);
            let textarea = make_textarea_with_text("", &vim_state, &theme);
            Self {
                _root: root,
                owner,
                state,
                config: crate::test_support::test_run_config(),
                action_tx,
                action_rx,
                pending: bridge::PendingWorkflowNotifications::new(),
                theme,
                vim_state,
                textarea,
                presentation: presentation(),
            }
        }

        /// The user types `text` in the composer and presses Enter, which
        /// sends it while the conversation is idle.
        fn type_and_press_enter(&mut self, text: &str) {
            self.textarea = make_textarea_with_text(text, &self.vim_state, &self.theme);
            let shared = Arc::new(Mutex::new(self.config.clone()));
            assert!(handle_idle_submit(
                &mut self.textarea,
                &mut self.vim_state,
                &self.theme,
                &mut self.state,
                &mut self.config,
                &shared,
                &self.action_tx,
            ));
            assert_eq!(textarea_text(&self.textarea), "", "the composer is cleared");
        }

        /// The user types `text` in the composer and presses Enter, or Ctrl+Enter,
        /// while a turn runs, which queues it behind that turn or sends it now.
        fn type_and_press_while_running(&mut self, text: &str, modifiers: KeyModifiers) {
            self.textarea = make_textarea_with_text(text, &self.vim_state, &self.theme);
            let shared = Arc::new(Mutex::new(self.config.clone()));
            let key = KeyEvent::new(KeyCode::Enter, modifiers);
            assert!(handle_running_key(
                &Event::Key(key),
                &key,
                &mut self.state,
                &mut self.config,
                &shared,
                &self.action_tx,
                &mut self.textarea,
                &mut self.vim_state,
                &self.theme,
            ));
            assert_eq!(textarea_text(&self.textarea), "", "the composer is cleared");
        }

        fn receive(&mut self, event: TuiEvent) {
            self.owner.handle(
                event,
                &mut self.state,
                &mut self.config,
                &self.action_tx,
                &self.pending,
                &mut self.textarea,
                &mut self.vim_state,
                &self.theme,
                &mut self.presentation,
            );
        }

        /// What was sent to the runtime since the last call.
        fn sent(&self) -> Vec<UserAction> {
            self.action_rx.try_iter().collect()
        }

        /// What the conversation shows of the messages the user sent.
        fn user_messages(&self) -> Vec<&str> {
            self.state
                .transcript
                .messages
                .iter()
                .filter_map(|message| match message {
                    ChatMessage::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }

        /// What is held back, shown above the composer.
        fn held(&self) -> Vec<&str> {
            self.state
                .held_submissions
                .iter()
                .map(HeldSubmission::visible_text)
                .collect()
        }
    }

    impl Drop for ResumingTui {
        fn drop(&mut self) {
            self.owner.shutdown();
        }
    }

    fn history_loaded(history: Option<&str>, label: &str) -> TuiEvent {
        TuiEvent::HistoryLoaded {
            messages: history
                .map(|text| ChatMessage::Assistant(text.to_string()))
                .into_iter()
                .collect(),
            plan: None,
            label: label.to_string(),
        }
    }

    /// What the controller says of a message it refused: the rejection, and
    /// that no turn started, with the token the message carried, if any.
    fn refused(prompt: &str, token: Option<SubmitToken>) -> [TuiEvent; 2] {
        [
            TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt: prompt.to_string(),
                bindings: Default::default(),
                images: Vec::new(),
                message: "could not start".to_string(),
            },
            TuiEvent::TurnNotStarted { token },
        ]
    }

    /// The token of the message `action` submits.
    fn token_of(action: &UserAction) -> SubmitToken {
        match action {
            UserAction::SubmitWithMentions {
                token: Some(token), ..
            } => *token,
            other => panic!("expected a submit with a token, got {other:?}"),
        }
    }

    /// The operation of the submit named `token` is active.
    fn active(token: SubmitToken) -> TuiEvent {
        TuiEvent::OperationActive { token: Some(token) }
    }

    /// An operation that is not the turn of a message the renderer named: a
    /// command that was ahead of it in the controller's line.
    fn active_without_a_token() -> TuiEvent {
        TuiEvent::OperationActive { token: None }
    }

    /// The queue the runtime keeps once it has what `action` queued, and the
    /// id of the turn it will start for it.
    fn queue_after(action: &UserAction) -> (PromptQueueSnapshot, String) {
        let UserAction::QueuePrompt {
            prompt,
            bindings,
            images,
        } = action
        else {
            panic!("expected a queued prompt, got {action:?}");
        };
        let item = QueuedSubmission {
            id: QueuedSubmissionId::new(),
            client_user_message_id: ClientUserMessageId::new(),
            input: PromptQueueInput {
                text: prompt.clone(),
                mention_bindings: bindings.clone(),
                images: ComposerImageState::image_inputs(images),
            },
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let turn_id = item.client_user_message_id.turn_id().to_string();
        (
            PromptQueueSnapshot {
                items: vec![item],
                ..Default::default()
            },
            turn_id,
        )
    }

    /// What the prompt given on the command line was sent as, once the
    /// history had loaded: a submit with a token.
    fn prompt_sent(sent: &[UserAction], prompt: &str) -> SubmitToken {
        let [action] = sent else {
            panic!("expected the prompt alone, got {sent:?}");
        };
        let UserAction::SubmitWithMentions {
            prompt: sent_prompt,
            token: Some(token),
            ..
        } = action
        else {
            panic!("expected a submit with a token, got {action:?}");
        };
        assert_eq!(sent_prompt, prompt);
        *token
    }

    #[test]
    fn held_submissions_follow_the_prompt_once_the_resumed_history_loads() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));

        tui.type_and_press_enter("typed");
        assert!(
            tui.sent().is_empty(),
            "a message sent before the history is held"
        );
        assert_eq!(tui.held(), ["typed"], "the held message stays on screen");
        assert!(
            tui.user_messages().is_empty(),
            "the history is about to replace the conversation"
        );
        assert_eq!(
            tui.state.status,
            AppStatus::Idle,
            "a held message starts no turn, so the next one is held as well"
        );

        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));

        // The prompt starts its turn alone. A message queued now would wait in
        // the controller's own line, behind that whole turn, where nothing
        // shows it, so the message waits for the turn's operation to be active.
        let sent = tui.sent();
        let token = prompt_sent(&sent, "cli");
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [
                ChatMessage::Assistant(history),
                ChatMessage::System { text: label, .. },
                ChatMessage::User(prompt),
            ] if history == "history" && label == "Resumed saved conversation." && prompt == "cli"
        ));
        assert_eq!(tui.state.status, AppStatus::Running);
        assert!(!tui.state.startup_history_pending);
        assert_eq!(tui.state.startup_turn, Some(token));
        assert_eq!(tui.held(), ["typed"], "still on screen, and only once");

        // The operation is active: the message queues behind it, in the
        // runtime's queue, which shows it.
        tui.receive(active(token));
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::QueuePrompt { prompt, .. }] if prompt == "typed"
            ),
            "{sent:?}"
        );
        assert!(tui.state.held_submissions.is_empty());
        assert_eq!(tui.state.startup_turn, None);

        let (queue, turn_id) = queue_after(&sent[0]);
        tui.receive(TuiEvent::PromptQueueUpdated(queue));
        assert!(tui.state.queued_follow_up_pending_or_in_flight());
        assert_eq!(
            tui.state
                .queued_submission_view()
                .map(|view| view.preview.first),
            Some("typed".to_string())
        );

        // The prompt's turn ends and the runtime starts the queued message's:
        // it reads after the prompt, once.
        tui.receive(TuiEvent::RuntimeTurnStarted { turn_id });
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [
                ChatMessage::Assistant(history),
                ChatMessage::System { .. },
                ChatMessage::User(prompt),
                ChatMessage::User(typed),
            ] if history == "history" && prompt == "cli" && typed == "typed"
        ));
    }

    #[test]
    fn held_submissions_follow_the_prompt_when_the_resume_fails() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));

        tui.type_and_press_enter("typed");
        assert!(tui.sent().is_empty());

        // The conversation could not be opened: what comes is an empty
        // history, and the prompt and the held message go to a new one.
        tui.receive(history_loaded(
            None,
            "Unable to restore saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [
                ChatMessage::System { text: label, .. },
                ChatMessage::User(prompt),
            ] if label == "Unable to restore saved conversation." && prompt == "cli"
        ));
        assert_eq!(tui.held(), ["typed"]);

        tui.receive(active(token));
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::QueuePrompt { prompt, .. }] if prompt == "typed"
            ),
            "{sent:?}"
        );
        assert!(!tui.state.holds_submissions());

        let (queue, turn_id) = queue_after(&sent[0]);
        tui.receive(TuiEvent::PromptQueueUpdated(queue));
        tui.receive(TuiEvent::RuntimeTurnStarted { turn_id });
        assert_eq!(tui.user_messages(), ["cli", "typed"]);
    }

    #[test]
    fn a_message_sent_once_the_history_has_loaded_is_not_held() {
        let mut tui = ResumingTui::waiting_for_history(None);
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        assert!(!tui.state.holds_submissions(), "no turn waits to start");

        tui.type_and_press_enter("later");

        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::SubmitWithMentions { prompt, token: None, .. }] if prompt == "later"
        ));
        assert_eq!(tui.user_messages(), ["later"]);
        assert!(tui.state.held_submissions.is_empty());
    }

    #[test]
    fn without_a_prompt_the_first_held_message_starts_its_turn_and_the_next_queues_behind_it() {
        let mut tui = ResumingTui::waiting_for_history(None);
        tui.type_and_press_enter("first");
        tui.type_and_press_enter("second");
        assert!(tui.sent().is_empty(), "the second one is held as well");
        assert_eq!(tui.held(), ["first", "second"]);

        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));

        // No prompt: the first message starts the turn, as a message sent
        // between turns does, and the second waits for it to be active.
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::SubmitWithMentions { prompt, token: Some(_), .. }] if prompt == "first"
            ),
            "{sent:?}"
        );
        let token = token_of(&sent[0]);
        assert_eq!(tui.user_messages(), ["first"]);
        assert_eq!(tui.held(), ["second"]);
        assert_eq!(tui.state.status, AppStatus::Running);

        tui.receive(active(token));
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::QueuePrompt { prompt, .. }] if prompt == "second"
            ),
            "{sent:?}"
        );
        assert!(!tui.state.holds_submissions());
    }

    #[test]
    fn a_stale_history_releases_nothing_and_a_later_one_does_not_release_again() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        let stale = SessionAttachmentId::new(1);
        let active_attachment = stale.next();
        tui.receive(attached(
            active_attachment,
            TuiEvent::SessionAttachmentActivated,
        ));
        tui.type_and_press_enter("typed");

        // The history of a conversation the TUI has left is not the one that
        // is waited for.
        tui.receive(attached(
            stale,
            history_loaded(Some("stale"), "stale history"),
        ));
        assert!(tui.sent().is_empty());
        assert!(tui.state.startup_history_pending);
        assert_eq!(tui.held(), ["typed"]);

        tui.receive(attached(
            active_attachment,
            history_loaded(Some("hydrated"), "loaded history"),
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        tui.receive(attached(active_attachment, active(token)));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "typed"
        ));
        assert!(!tui.state.holds_submissions());

        // A conversation the user switches to later has its history too: the
        // messages are not sent into it again.
        tui.receive(attached(
            active_attachment,
            history_loaded(Some("another"), "loaded another"),
        ));
        assert!(tui.sent().is_empty());
        assert!(!tui.state.holds_submissions());
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [ChatMessage::Assistant(history), ChatMessage::System { .. }] if history == "another"
        ));
    }

    fn a_new_conversation_projection() -> TuiEvent {
        // `/new` starts the session with a projection of its own, which
        // clears the screen.
        TuiEvent::SessionProjectionReset(Box::new(
            crate::surface_projection::SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("session-2".to_string()),
                title: "New conversation".to_string(),
                usage_revision: 0,
                usage: Default::default(),
                context_revision: 0,
                context_used_tokens: 0,
                context_limit_tokens: 0,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
                mcp_catalog: Default::default(),
            },
        ))
    }

    #[test]
    fn a_new_conversation_that_takes_the_place_of_the_resume_starts_the_held_messages() {
        let mut tui = ResumingTui::waiting_for_history(None);
        tui.type_and_press_enter("typed");
        tui.type_and_press_enter("then");

        tui.receive(a_new_conversation_projection());
        assert!(tui.sent().is_empty());
        tui.receive(TuiEvent::NewSessionStarted);

        // No history will come for it: the first message starts its turn,
        // the next waits for that turn's operation to be active.
        assert!(!tui.state.startup_history_pending);
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::SubmitWithMentions { prompt, token: Some(_), .. }] if prompt == "typed"
            ),
            "{sent:?}"
        );
        assert_eq!(tui.held(), ["then"]);
        tui.receive(active(token_of(&sent[0])));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "then"
        ));
        assert!(!tui.state.holds_submissions());
    }

    #[test]
    fn a_new_conversation_that_takes_the_place_of_the_resume_gets_the_prompt_first_and_leaves_none_pending()
     {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("typed");

        // `/new` before the history: the conversation the prompt was given
        // for is not the one that will be shown, and the prompt is not left
        // for whatever history comes next.
        tui.receive(a_new_conversation_projection());
        tui.receive(TuiEvent::NewSessionStarted);

        // The prompt goes first, the held message after its turn is active.
        let token = prompt_sent(&tui.sent(), "cli");
        assert!(tui.owner.pending_initial_prompt.is_none());
        assert_eq!(tui.user_messages(), ["cli"]);
        assert_eq!(tui.held(), ["typed"]);
        assert!(tui.state.holds_submissions());
        tui.receive(active(token));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "typed"
        ));
        assert!(!tui.state.holds_submissions());

        // A history that comes later, another conversation's, is just that:
        // the prompt is not sent into it.
        tui.receive(history_loaded(
            Some("another"),
            "Resumed saved conversation.",
        ));
        assert!(tui.sent().is_empty());
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [ChatMessage::Assistant(history), ChatMessage::System { .. }] if history == "another"
        ));
    }

    #[test]
    fn a_new_conversation_after_the_history_does_not_send_the_prompt_again() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        tui.receive(active(token));

        tui.receive(a_new_conversation_projection());
        tui.receive(TuiEvent::NewSessionStarted);

        assert!(tui.sent().is_empty());
        assert!(!tui.state.holds_submissions());
    }

    #[test]
    fn what_is_sent_while_the_prompts_turn_starts_is_held_too() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("before");
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        assert_eq!(tui.state.status, AppStatus::Running);

        // The turn runs as far as the TUI knows, so Enter queues. Queued now,
        // it would sit in the controller's line behind the whole turn, out of
        // the queue's sight and behind whatever is sent once the turn is
        // under way, so it is held with the one before it.
        tui.type_and_press_while_running("A", KeyModifiers::NONE);
        tui.type_and_press_while_running("B", KeyModifiers::CONTROL);
        assert!(tui.sent().is_empty());
        assert_eq!(tui.held(), ["before", "A", "B"]);

        tui.receive(active(token));
        let sent = tui.sent();
        let queued: Vec<&str> = sent
            .iter()
            .map(|action| match action {
                UserAction::QueuePrompt { prompt, .. } => prompt.as_str(),
                other => panic!("expected queued prompts, got {other:?}"),
            })
            .collect();
        assert_eq!(queued, ["before", "A", "B"], "in the order they were sent");

        // From then on a message queues at once, as always.
        tui.type_and_press_while_running("C", KeyModifiers::NONE);
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "C"
        ));
    }

    #[test]
    fn a_command_typed_before_the_history_does_not_stand_in_for_the_prompts_turn() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        // `/compact`, `$skill` or `/goal`, typed before the history: it is
        // ahead of the prompt in the controller's line, which runs the
        // conversation's startup first, and the TUI is "running" for it.
        tui.state.enter_running();
        tui.type_and_press_while_running("A", KeyModifiers::NONE);
        tui.type_and_press_while_running("B", KeyModifiers::NONE);
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        // Everything sent to the controller, in order.
        let mut sent = tui.sent();
        let token = prompt_sent(&sent, "cli");

        // The command's own operation is active before the prompt's, and says
        // so without a token: the messages are not for it.
        tui.receive(active_without_a_token());
        assert!(
            tui.sent().is_empty(),
            "the command's operation released the held messages"
        );
        assert_eq!(tui.held(), ["A", "B"]);
        assert!(tui.state.holds_submissions());
        // What it does after that changes nothing either: the conversation is
        // idle again, and what is typed now is held behind them.
        tui.receive(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(tui.sent().is_empty());
        assert_eq!(tui.held(), ["A", "B"]);
        tui.type_and_press_enter("C");
        assert!(tui.sent().is_empty());
        assert_eq!(tui.held(), ["A", "B", "C"]);

        // The prompt's own operation is active: the messages queue behind it,
        // in the order they were sent.
        tui.receive(active(token));
        sent.extend(tui.sent());
        assert!(
            matches!(
                sent.as_slice(),
                [
                    UserAction::SubmitWithMentions { prompt: cli, token: Some(_), .. },
                    UserAction::QueuePrompt { prompt: a, .. },
                    UserAction::QueuePrompt { prompt: b, .. },
                    UserAction::QueuePrompt { prompt: c, .. },
                ] if cli == "cli" && a == "A" && b == "B" && c == "C"
            ),
            "{sent:?}"
        );
        assert!(!tui.state.holds_submissions());
    }

    #[test]
    fn a_refused_command_typed_before_the_history_starts_none_of_the_held_messages() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("A");
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");

        // A `$skill` typed before the history is refused: a plain submit that
        // did not start a turn, ahead of the prompt in the controller's line.
        for event in refused("$skill", None) {
            tui.receive(event);
        }
        assert!(tui.sent().is_empty(), "{:?}", tui.sent());
        assert_eq!(tui.held(), ["A"]);
        assert_eq!(tui.state.startup_turn, Some(token));

        // The prompt is what the held messages wait for, and it starts.
        tui.receive(active(token));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "A"
        ));
        assert!(!tui.state.holds_submissions());
    }

    #[test]
    fn the_events_of_another_submit_change_nothing() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("A");
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        let other = SubmitToken::new(4_000);

        tui.receive(TuiEvent::OperationActive { token: Some(other) });
        tui.receive(TuiEvent::TurnNotStarted { token: Some(other) });

        assert!(tui.sent().is_empty());
        assert_eq!(tui.held(), ["A"]);
        assert_eq!(tui.state.startup_turn, Some(token));
    }

    #[test]
    fn a_start_that_does_not_happen_does_not_hold_the_rest_for_good() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("A");
        tui.type_and_press_enter("B");
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let cli = prompt_sent(&tui.sent(), "cli");

        // The prompt is refused before its operation is active: the first
        // message held starts its turn instead, under a token of its own, and
        // the other waits for that.
        for event in refused("cli", Some(cli)) {
            tui.receive(event);
        }
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::SubmitWithMentions { prompt, token: Some(_), .. }] if prompt == "A"
            ),
            "{sent:?}"
        );
        let a = token_of(&sent[0]);
        assert_ne!(a, cli);
        assert_eq!(tui.held(), ["B"]);
        assert_eq!(tui.state.startup_turn, Some(a));

        // What the refused prompt says late is not about A.
        tui.receive(TuiEvent::TurnNotStarted { token: Some(cli) });
        tui.receive(active(cli));
        assert!(tui.sent().is_empty());
        assert_eq!(tui.held(), ["B"]);

        // So is that one refused: the next message held starts its turn.
        for event in refused("A", Some(a)) {
            tui.receive(event);
        }
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::SubmitWithMentions { prompt, token: Some(_), .. }] if prompt == "B"
            ),
            "{sent:?}"
        );
        let b = token_of(&sent[0]);
        assert!(tui.held().is_empty());
        assert_eq!(tui.state.startup_turn, Some(b));

        // Nothing is left to wait for, and what comes next is not held.
        for event in refused("B", Some(b)) {
            tui.receive(event);
        }
        assert!(!tui.state.holds_submissions());
        assert!(tui.sent().is_empty());
        tui.type_and_press_enter("C");
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::SubmitWithMentions { prompt, token: None, .. }] if prompt == "C"
        ));
    }

    #[test]
    fn a_start_that_does_not_happen_and_one_that_does_release_the_rest_behind_the_latter() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.type_and_press_enter("A");
        tui.type_and_press_enter("B");
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let cli = prompt_sent(&tui.sent(), "cli");

        for event in refused("cli", Some(cli)) {
            tui.receive(event);
        }
        let sent = tui.sent();
        assert!(matches!(
            sent.as_slice(),
            [UserAction::SubmitWithMentions { prompt, .. }] if prompt == "A"
        ));
        let a = token_of(&sent[0]);

        // A's turn is under way: B queues behind it.
        tui.receive(active(a));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::QueuePrompt { prompt, .. }] if prompt == "B"
        ));
        assert!(!tui.state.holds_submissions());
        // The turn's own events change nothing.
        tui.receive(TuiEvent::TurnNotStarted { token: Some(a) });
        assert!(tui.sent().is_empty());
    }

    #[test]
    fn a_prompt_that_does_not_start_ends_the_hold_when_nothing_is_held() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        let token = prompt_sent(&tui.sent(), "cli");
        assert!(tui.state.holds_submissions());

        // The runtime had the prompt queued behind a turn it was running.
        tui.receive(TuiEvent::TurnNotStarted { token: Some(token) });

        assert!(!tui.state.holds_submissions());
        assert!(tui.sent().is_empty());
    }

    #[test]
    fn what_says_no_turn_waits_to_start_or_become_active_changes_nothing() {
        let mut tui = ResumingTui::waiting_for_history(None);
        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));
        assert!(!tui.state.holds_submissions());

        tui.receive(active_without_a_token());
        tui.receive(TuiEvent::TurnNotStarted { token: None });
        tui.receive(active(SubmitToken::new(1)));
        tui.receive(TuiEvent::TurnNotStarted {
            token: Some(SubmitToken::new(1)),
        });

        assert!(tui.sent().is_empty());
        assert!(!tui.state.holds_submissions());
        assert!(tui.state.held_submissions.is_empty());
    }
}
