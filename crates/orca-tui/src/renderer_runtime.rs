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
use crate::idle_submit_actions::{handle_idle_submit, release_held_submissions};
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
            TuiEvent::HistoryLoaded { .. } => {
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
                if let Some(prompt) = self.pending_initial_prompt.take() {
                    state.push_message(ChatMessage::User(prompt.clone()));
                    state.enter_running();
                    let _ = action_tx.send(UserAction::Submit(prompt));
                }
                // What the user sent meanwhile comes after the prompt.
                release_held_submissions(state, action_tx);
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
            TuiEvent::NewSessionStarted => {
                config.history_mode = HistoryMode::Record;
                handle_runtime_event(
                    TuiEvent::NewSessionStarted,
                    state,
                    action_tx,
                    pending_workflow_notifications,
                    textarea,
                    vim_state,
                    theme,
                    presentation,
                );
                // A new conversation that takes the place of the one resumed
                // at launch has no history to wait for.
                release_held_submissions(state, action_tx);
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
    use crate::protocol::{AttachedTuiEvent, TuiEvent, UserAction};
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
            Ok(UserAction::Submit(prompt)) if prompt == "follow up"
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

        /// The user types `text` in the composer and presses Enter.
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

    #[test]
    fn held_submissions_follow_the_prompt_once_the_resumed_history_loads() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));

        tui.type_and_press_enter("typed");
        assert!(
            tui.sent().is_empty(),
            "a message sent before the history is held"
        );
        assert_eq!(
            tui.user_messages(),
            ["typed"],
            "the held message stays on screen"
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

        // The prompt runs first; the held message waits behind its turn in the
        // queue, as one typed while that turn runs does.
        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::Submit(prompt), UserAction::QueuePrompt { prompt: queued, .. }]
                    if prompt == "cli" && queued == "typed"
            ),
            "{sent:?}"
        );
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
        assert!(tui.state.held_submissions.is_empty());

        // The runtime has the message queued behind the prompt's turn, and the
        // queue strip shows it there.
        let (queue, turn_id) = queue_after(&sent[1]);
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

        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [UserAction::Submit(prompt), UserAction::QueuePrompt { prompt: queued, .. }]
                    if prompt == "cli" && queued == "typed"
            ),
            "{sent:?}"
        );
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [
                ChatMessage::System { text: label, .. },
                ChatMessage::User(prompt),
            ] if label == "Unable to restore saved conversation." && prompt == "cli"
        ));
        assert!(!tui.state.startup_history_pending);

        let (queue, turn_id) = queue_after(&sent[1]);
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
        assert!(!tui.state.startup_history_pending);

        tui.type_and_press_enter("later");

        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::SubmitWithMentions { prompt, .. }] if prompt == "later"
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
        assert_eq!(tui.user_messages(), ["first", "second"]);

        tui.receive(history_loaded(
            Some("history"),
            "Resumed saved conversation.",
        ));

        let sent = tui.sent();
        assert!(
            matches!(
                sent.as_slice(),
                [
                    UserAction::SubmitWithMentions { prompt: first, .. },
                    UserAction::QueuePrompt { prompt: second, .. },
                ] if first == "first" && second == "second"
            ),
            "{sent:?}"
        );
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [
                ChatMessage::Assistant(_),
                ChatMessage::System { .. },
                ChatMessage::User(first),
            ] if first == "first"
        ));
        assert_eq!(tui.state.status, AppStatus::Running);
    }

    #[test]
    fn a_stale_history_releases_nothing_and_a_later_one_does_not_release_again() {
        let mut tui = ResumingTui::waiting_for_history(Some("cli"));
        let stale = SessionAttachmentId::new(1);
        let active = stale.next();
        tui.receive(attached(active, TuiEvent::SessionAttachmentActivated));
        tui.type_and_press_enter("typed");

        // The history of a conversation the TUI has left is not the one that
        // is waited for.
        tui.receive(attached(
            stale,
            history_loaded(Some("stale"), "stale history"),
        ));
        assert!(tui.sent().is_empty());
        assert!(tui.state.startup_history_pending);
        assert_eq!(tui.state.held_submissions.len(), 1);

        tui.receive(attached(
            active,
            history_loaded(Some("hydrated"), "loaded history"),
        ));
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::Submit(prompt), UserAction::QueuePrompt { prompt: queued, .. }]
                if prompt == "cli" && queued == "typed"
        ));
        assert!(!tui.state.startup_history_pending);

        // A conversation the user switches to later has its history too: the
        // messages are not sent into it again.
        tui.receive(attached(
            active,
            history_loaded(Some("another"), "loaded another"),
        ));
        assert!(tui.sent().is_empty());
        assert!(matches!(
            tui.state.transcript.messages.as_slice(),
            [ChatMessage::Assistant(history), ChatMessage::System { .. }] if history == "another"
        ));
    }

    #[test]
    fn a_new_conversation_that_takes_the_place_of_the_resume_releases_the_held_messages() {
        let mut tui = ResumingTui::waiting_for_history(None);
        tui.type_and_press_enter("typed");

        // `/new` before the history: the session starts with a projection of
        // its own, which clears the screen, and is announced.
        tui.receive(TuiEvent::SessionProjectionReset(Box::new(
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
        )));
        assert!(tui.sent().is_empty());
        tui.receive(TuiEvent::NewSessionStarted);

        // No history will come for it: what was held is sent to it, once.
        assert!(!tui.state.startup_history_pending);
        assert!(tui.state.held_submissions.is_empty());
        assert!(matches!(
            tui.sent().as_slice(),
            [UserAction::SubmitWithMentions { prompt, .. }] if prompt == "typed"
        ));
        assert_eq!(tui.user_messages(), ["typed"]);
    }
}
