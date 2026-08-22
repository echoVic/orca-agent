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
use crate::idle_submit_actions::handle_idle_submit;
use crate::mention_search_manager::MentionSearchManager;
use crate::queued_input_actions::enqueue_composer_follow_up_to_runtime;
use crate::runtime_event_actions::handle_runtime_event;
use crate::surface_actions::TuiSurfaceActions;
use crate::terminal_presentation::TerminalPresentation;
use crate::theme::Theme;
use crate::types::{AppState, ChatMessage, TuiEvent, UserAction};
use crate::vim::VimState;
use crate::workspace_config::mention_search_roots;

pub(crate) struct RendererRuntimeEventOwner {
    mention_search: MentionSearchManager,
    pending_initial_prompt: Option<String>,
}

impl RendererRuntimeEventOwner {
    pub(crate) fn new(
        mention_search: MentionSearchManager,
        pending_initial_prompt: Option<String>,
    ) -> Self {
        Self {
            mention_search,
            pending_initial_prompt,
        }
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
            }
            TuiEvent::SettingsUpdated {
                model,
                reasoning_effort,
                approval_mode,
            } => {
                config.model =
                    orca_core::model::ModelSelection::from_unchecked(Some(model.clone()));
                config.reasoning_effort = reasoning_effort;
                config.approval_mode = approval_mode;
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
    use crossbeam_channel as mpsc;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{ReasoningEffort, ThemeName};
    use tui_textarea::TextArea;

    use super::RendererRuntimeEventOwner;
    use crate::bridge;
    use crate::mention_search_manager::MentionSearchManager;
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::theme::Theme;
    use crate::types::{
        AppState, AppStatus, AttachedTuiEvent, ChatMessage, SessionAttachmentId, TuiEvent,
        UserAction,
    };
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
        assert!(state.messages.is_empty());

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
            state.messages.as_slice(),
            [
                ChatMessage::Assistant(history),
                ChatMessage::System(label),
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
        assert_eq!(state.model_name, "deepseek-reasoner");
        assert_eq!(state.reasoning_effort, ReasoningEffort::High);
        assert_eq!(state.approval_mode, ApprovalMode::FullAuto);
        owner.shutdown();
    }
}
