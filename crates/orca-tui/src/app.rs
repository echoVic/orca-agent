use std::io::{self, Write};
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use crossbeam_channel as mpsc;
#[cfg(test)]
use crossterm::event::{Event, KeyEvent, KeyModifiers};

#[cfg(test)]
use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
#[cfg(test)]
use orca_core::conversation::Message;
use orca_runtime::history;
#[cfg(test)]
use orca_runtime::runtime_host::HostedOperationKind;
#[cfg(test)]
use orca_runtime::runtime_host::{RuntimeThreadHandle, RuntimeThreadStartRequest};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::agent_runtime::TuiAgentRuntime;
#[cfg(test)]
use crate::attachment_routing::{
    AttachmentRouting, accept_attached_tui_event, reduce_attached_tui_event,
    rotate_attached_event_sender, spawn_attached_event_sender,
    spawn_attached_event_sender_with_routing,
};
#[cfg(test)]
use crate::background_tasks::notify_recovered_background_approvals_for_tui;
use crate::bridge;
use crate::channels::{tui_event_channel, user_action_channel};
use crate::clipboard;
use crate::composer_textarea::{make_setup_textarea, make_textarea};
use crate::exit_policy::TuiExit;
use crate::exit_policy::{exit_resume_hint, exit_session_id};
#[cfg(test)]
use crate::frame_scheduler::{FrameScheduler, IterationEvent, run_event_loop_iteration};
use crate::hosted_controller::hosted_tui_controller_loop;
#[cfg(test)]
use crate::hosted_goal::run_hosted_goal_run;
#[cfg(test)]
use crate::hosted_runtime::TuiHostedOperationOutcome;
#[cfg(test)]
use crate::hosted_runtime::emit_hosted_operation_error;
#[cfg(test)]
use crate::hosted_runtime::{hosted_turn_request, run_hosted_ordinary_turn, send_submission_error};
use crate::hosted_session::typed_history_startup_eligible;
#[cfg(test)]
use crate::hosted_session::{announce_runtime_ready, emit_typed_history_snapshot};
#[cfg(test)]
use crate::hosted_session::{chat_message_from_history, load_saved_history_fallback};
#[cfg(all(test, unix))]
use crate::hosted_session_lifecycle::ensure_hosted_thread;
#[cfg(test)]
use crate::hosted_session_lifecycle::preflight_started_session;
#[cfg(all(test, unix))]
use crate::hosted_session_lifecycle::start_new_hosted_session;
#[cfg(test)]
use crate::hosted_side::{HostedSideParent, shutdown_attached_side_on_controller_exit};
#[cfg(test)]
use crate::hosted_submission::handle_hosted_submitted_turn;
#[cfg(test)]
use crate::input_event_actions::handle_paste_event;
#[cfg(test)]
use crate::input_runtime::InputControl;
#[cfg(test)]
use crate::input_wake::{
    InputWake, receive_input_batch, receive_input_or_control, receive_prioritized_input_or_control,
};
#[cfg(test)]
use crate::insert_escape::flush_expired_insert_escape;
#[cfg(test)]
use crate::insert_escape::{
    PendingInsertEscapeRouting, flush_pending_insert_escape_before_non_key,
    resolve_pending_insert_escape_before_routing,
};
#[cfg(test)]
use crate::key_event_actions::{KeyEventFlow, handle_key_event_preflight};
use crate::mention_search_manager::MentionSearchManager;
use crate::operation_controller::TuiSurfaceTaskControl;
#[cfg(test)]
use crate::protocol::AttachedTuiEvent;
#[cfg(test)]
use crate::protocol::SessionAttachmentId;
#[cfg(test)]
use crate::protocol::TuiEvent;
use crate::protocol::UserAction;
use crate::renderer_interaction_acks::RendererInteractionAckOwner;
use crate::renderer_loop::RendererLoopOwner;
use crate::renderer_runtime::RendererRuntimeEventOwner;
use crate::renderer_runtime_inbox::RendererRuntimeInboxOwner;
#[cfg(test)]
use crate::runtime_event_actions::handle_runtime_event;
use crate::scrollback::clear_terminal_scrollback;
#[cfg(test)]
use crate::scrollback::clear_terminal_scrollback_with;
#[cfg(test)]
use crate::status_key_actions::handle_status_key;
#[cfg(test)]
use crate::submitted_turn::SubmittedTurn;
#[cfg(test)]
use crate::surface_actions::TuiSurfaceActions;
#[cfg(test)]
use crate::surface_projection::SessionProjectionPresentation;
#[cfg(test)]
use crate::surface_projection::SurfaceProjectionState;
#[cfg(test)]
use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
use crate::terminal_session::PendingTerminalSession;
#[cfg(test)]
use crate::theme::Theme;
use crate::transcript_state::ChatMessage;
use crate::tui_run_lifecycle::finish_tui_run;
#[cfg(test)]
use crate::types::SideParentStatus;
use crate::types::{AppState, AppStatus};
use crate::ui;
use crate::vim::VimState;
#[cfg(test)]
use crate::workspace_config::{configure_and_preload_tui_state, configure_tui_syntax_state};
use crate::workspace_config::{mention_search_roots, syntax_workspace_root};
use crate::workspace_status;

pub fn run_tui(config: RunConfig) -> i32 {
    match run_tui_inner(config) {
        Ok(exit) => {
            if let Some(hint) = exit_resume_hint(exit.session_id.as_deref()) {
                let _ = io::stdout().lock().write_all(hint.as_bytes());
            }
            exit.code
        }
        Err(e) => {
            eprintln!("TUI error: {e}");
            1
        }
    }
}

fn run_tui_inner(mut config: RunConfig) -> io::Result<TuiExit> {
    let pending_terminal_session =
        PendingTerminalSession::start(config.theme, config.terminal_notifications)?;

    const FRAME_INTERVAL: Duration = Duration::from_millis(16);
    const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
    const MAX_INPUT_EVENTS_PER_BATCH: usize = 64;
    const MAX_RUNTIME_EVENTS_PER_BATCH: usize = crate::channels::TUI_EVENT_CAPACITY;
    const MAX_SUPERVISED_TUI_TASKS: usize = 32;

    let workspace_root = syntax_workspace_root(&config);
    let (event_tx, pending_event_rx) = tui_event_channel();
    let (action_tx, action_rx) = user_action_channel();
    let mention_search = MentionSearchManager::new_roots(
        mention_search_roots(&config, &workspace_root),
        event_tx.clone(),
    );
    let pending_workflow_notifications: bridge::PendingWorkflowNotifications =
        bridge::PendingWorkflowNotifications::new();

    let model_name = config.model.display_name().to_string();

    let needs_setup = config.api_key.is_none();
    let should_show_picker = config.show_session_picker
        && !needs_setup
        && config.prompt.trim().is_empty()
        && !matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::Fork(_)
        );
    let picker_page = if should_show_picker {
        RuntimeSurfaceHostHandle::list_saved_session_page(
            0,
            crate::session_picker_actions::SESSION_PICKER_PAGE_SIZE,
            None,
        )
        .ok()
    } else {
        None
    };

    let workspace_status = workspace_status::snapshot(&workspace_root);
    let mut state = AppState::new(
        action_tx.clone(),
        config.app_version.clone(),
        model_name,
        workspace_status.cwd,
    );
    state.workspace_git = workspace_status.git;
    state.approval_mode = config.approval_mode;
    state.reasoning_effort = config.reasoning_effort;
    if let Some(page) = picker_page
        && !page.sessions.is_empty()
    {
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = page.sessions;
        state.session_picker_next_offset = page.next_offset;
        state.session_picker_backfill_complete = page.backfill_complete;
    }

    if needs_setup {
        state.status = AppStatus::Setup;
        state.setup_step = 0;
    }

    let initial_prompt = if config.prompt.trim().is_empty() {
        None
    } else {
        Some(config.prompt.clone())
    };

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    // Session selectors are resolved by RuntimeHost. Keeping this lane empty
    // prevents the TUI from becoming the history owner before the runtime
    // can establish its lease and typed surface.
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(None));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let agent_workflow_notifications = pending_workflow_notifications.clone();
    let agent_controller = TuiSurfaceTaskControl::new();

    let mut agent_runtime = match TuiAgentRuntime::spawn_hosted(
        action_rx,
        event_tx.clone(),
        MAX_SUPERVISED_TUI_TASKS,
        agent_controller,
        move |agent_controller, command_rx, host| {
            hosted_tui_controller_loop(
                agent_config,
                agent_preloaded,
                agent_event_tx,
                command_rx,
                agent_controller,
                agent_workflow_notifications,
                host,
            );
        },
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            return pending_terminal_session.fail_after_agent_startup(error);
        }
    };
    let pending_initial_prompt =
        if typed_history_startup_eligible(&config.history_mode, &preloaded_transcript) {
            initial_prompt.clone()
        } else {
            None
        };
    let renderer_interaction_acks =
        RendererInteractionAckOwner::new(agent_runtime.interaction_ack_receiver());
    let renderer_runtime_inbox = RendererRuntimeInboxOwner::new(pending_event_rx);

    let mut vim_state =
        VimState::with_insert_escape(config.vim_mode, config.vim_insert_escape.clone());
    let mut textarea = if needs_setup {
        make_setup_textarea(pending_terminal_session.theme())
    } else {
        if let Some(prompt) = initial_prompt.clone()
            && pending_initial_prompt.is_none()
        {
            state.push_message(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        make_textarea(&vim_state, pending_terminal_session.theme())
    };
    let mut renderer_runtime =
        RendererRuntimeEventOwner::new(mention_search, pending_initial_prompt);

    let renderer_result = match pending_terminal_session.activate() {
        Ok(terminal_session) => terminal_session.run(
            MAX_INPUT_EVENTS_PER_BATCH,
            state.status,
            (&mut state, &mut textarea),
            |terminal, theme, context| {
                let (state, textarea) = context;
                terminal
                    .draw(|f| ui::render(f, state, textarea, theme))
                    .map(|_| ())
            },
            |terminal, presentation, renderer_input_wake, theme, context| {
                let (state, textarea) = context;
                let exit_code = RendererLoopOwner::new(
                    Instant::now(),
                    FRAME_INTERVAL,
                    ANIMATION_INTERVAL,
                    MAX_RUNTIME_EVENTS_PER_BATCH,
                    renderer_input_wake,
                    &renderer_interaction_acks,
                    &renderer_runtime_inbox,
                    &mut renderer_runtime,
                    state,
                    &mut config,
                    &shared_config,
                    &action_tx,
                    &pending_workflow_notifications,
                    &preloaded_transcript,
                    textarea,
                    &mut vim_state,
                    theme,
                    presentation,
                    &initial_prompt,
                    &workspace_root,
                )
                .run(
                    terminal,
                    clear_terminal_scrollback,
                    clipboard::copy_to_clipboard,
                    |terminal, presentation, status| {
                        let _ =
                            presentation.write_pending(terminal.backend_mut().inner_mut(), status);
                    },
                )?;
                Ok(exit_code)
            },
        ),
        Err(error) => Err(error),
    };
    let exit_code = finish_tui_run(
        renderer_result,
        || renderer_runtime.shutdown(),
        || renderer_runtime_inbox.shutdown(),
        || agent_runtime.shutdown(),
    )?;

    Ok(TuiExit {
        code: exit_code,
        session_id: exit_session_id(
            state.current_session_id().map(ToOwned::to_owned),
            &config.history_mode,
        ),
    })
}

#[cfg(test)]
fn run_manual_compaction_with_events(
    event_tx: &mpsc::Sender<TuiEvent>,
    compact: impl FnOnce() -> (usize, usize),
) {
    let _ = event_tx.send(TuiEvent::CompactionStarted);
    let (before_messages, after_messages) = compact();
    let _ = event_tx.send(TuiEvent::Compacted {
        before_messages,
        after_messages,
        reason: "manual".to_string(),
        strategy: "manual".to_string(),
        collapsed_messages: before_messages.saturating_sub(after_messages),
        status_text: "compacted context manually".to_string(),
    });
}

#[cfg(test)]
fn spawn_unwrapped_tui_test_event_sender(
    output_tx: mpsc::Sender<TuiEvent>,
) -> mpsc::Sender<TuiEvent> {
    let (event_tx, event_rx) = mpsc::unbounded();
    std::thread::Builder::new()
        .name("orca-tui-test-event-unwrapper".to_string())
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                let event = match event {
                    TuiEvent::Attached(attached) => {
                        let AttachedTuiEvent { event, .. } = *attached;
                        if matches!(event, TuiEvent::SessionAttachmentActivated) {
                            continue;
                        }
                        event
                    }
                    TuiEvent::SessionAttachmentActivated => continue,
                    event => event,
                };
                if output_tx.send(event).is_err() {
                    break;
                }
            }
        })
        .expect("spawn TUI test event unwrapper");
    event_tx
}

#[cfg(test)]
fn spawn_hosted_tui_test_runtime(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
) -> TuiAgentRuntime {
    spawn_hosted_tui_test_runtime_with_background_capacity(
        config, preloaded, event_tx, action_rx, 8,
    )
}

#[cfg(test)]
fn spawn_hosted_tui_test_runtime_with_background_capacity(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    background_capacity: usize,
) -> TuiAgentRuntime {
    let event_tx = spawn_unwrapped_tui_test_event_sender(event_tx);
    let pending = bridge::PendingWorkflowNotifications::new();
    let controller = TuiSurfaceTaskControl::new();
    let agent_config = Arc::clone(&config);
    let agent_preloaded = Arc::clone(&preloaded);
    let agent_events = event_tx.clone();
    let agent_pending = pending.clone();
    TuiAgentRuntime::spawn_hosted(
        action_rx,
        event_tx,
        background_capacity,
        controller,
        move |controller, commands, host| {
            hosted_tui_controller_loop(
                agent_config,
                agent_preloaded,
                agent_events,
                commands,
                controller,
                agent_pending,
                host,
            );
        },
    )
    .expect("hosted TUI test runtime")
}

#[cfg(test)]
fn run_hosted_tui_controller_for_test(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    _cancel: CancelToken,
    _pending_workflow_notifications: bridge::PendingWorkflowNotifications,
) {
    let mut runtime = spawn_hosted_tui_test_runtime(config, preloaded, event_tx, action_rx);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !runtime.controller().is_shutdown() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    runtime.shutdown().expect("hosted TUI test shutdown");
}

#[cfg(test)]
#[path = "app_integration_tests.rs"]
mod tests;
