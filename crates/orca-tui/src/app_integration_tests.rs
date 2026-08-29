use super::*;
use crossterm::event::KeyCode;
use orca_core::approval_types::ApprovalMode;
use orca_core::model::ModelSelection;

#[test]
fn exit_resume_hint_matches_claude_code_style() {
    assert_eq!(
        exit_resume_hint(Some("a11864d0-9e08-487a-b148-da0012879b66")),
        Some(
            "Resume this session with:\n\
                 orca --resume a11864d0-9e08-487a-b148-da0012879b66\n"
                .to_string()
        )
    );
}

#[test]
fn exit_resume_hint_is_absent_without_a_persisted_session() {
    assert_eq!(exit_resume_hint(None), None);
}

#[test]
fn exit_session_id_resolves_a_picker_selection_without_a_live_thread() {
    with_orca_home(|home| {
        let meta = history::create_meta(home, "mock", None, "resume after picker");
        let expected = meta.session_id.clone();
        history::SessionWriter::start_from_meta(meta).expect("saved session");

        assert_eq!(
            exit_session_id(None, &HistoryMode::Resume("latest".to_string())),
            Some(expected)
        );
    });
}

#[test]
fn exit_session_id_prefers_the_current_picker_selection() {
    with_orca_home(|home| {
        let meta = history::create_meta(home, "mock", None, "selected session");
        let expected = meta.session_id.clone();
        history::SessionWriter::start_from_meta(meta).expect("saved session");

        assert_eq!(
            exit_session_id(
                Some("11111111-1111-1111-1111-111111111111".to_string()),
                &HistoryMode::Resume("latest".to_string())
            ),
            Some(expected)
        );
    });
}
use tui_textarea::TextArea;

use crate::approval_actions::resolve_approval_option;
use crate::commands;
use crate::composer_textarea::{
    insert_composer_paste, insert_pasted_text, make_textarea_with_text, textarea_text,
};
use crate::idle_submit_actions::handle_idle_submit;
use crate::key_event_actions::handle_transcript_search_key;
use crate::protocol::PendingTuiInput;
use crate::protocol::{TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};
use crate::selection::{SelectionGranularity, SelectionPos, TranscriptSelection};
use crate::slash_command_actions::handle_slash_command;
use crate::types::{ApprovalOption, SlashMenu, SlashMenuItem, SubMenu};
use crate::workflow_notifications::drain_pending_workflow_notifications;
use crate::workflow_notifications::{
    is_workflow_notification_turn_boundary, queue_workflow_terminal_notification,
    remove_pending_workflow_notification_by_id, submit_pending_workflow_notification,
};
use crate::workflow_panel_actions::handle_workflows_panel_key;
use orca_core::config::{
    ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig, VimInsertEscapeSequence,
    WorkflowConfig,
};
use tempfile::tempdir;

fn vim_insert_input(character: char) -> tui_textarea::Input {
    tui_textarea::Input {
        key: tui_textarea::Key::Char(character),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

#[test]
fn receive_input_batch_waits_drains_and_caps() {
    let (sender, receiver) = mpsc::bounded(128);
    for character in 'a'..='z' {
        sender
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )))
            .expect("receiver alive");
    }

    let first = receive_input_batch(&receiver, Duration::from_millis(10), 5)
        .expect("queued input should be received");
    assert_eq!(first.len(), 5);
    assert_eq!(receiver.len(), 21);

    let remaining = receive_input_batch(&receiver, Duration::from_millis(10), 64)
        .expect("remaining queued input should be received");
    assert_eq!(remaining.len(), 21);
    assert!(receiver.is_empty());

    sender
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
        )))
        .expect("receiver alive");
    assert_eq!(
        receive_input_batch(&receiver, Duration::from_millis(10), 0),
        Ok(Vec::new())
    );
    assert_eq!(receiver.len(), 1);
}

#[test]
fn pending_insert_escape_preflight_precedes_shortcuts_only_after_sequence_started() {
    let theme = Theme::named(ThemeName::Dark);
    let sequence = VimInsertEscapeSequence::parse("jj").unwrap();
    let started = Instant::now();
    let mut vim = VimState::with_insert_escape(true, Some(sequence));
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    let mut state = test_state().0;
    let config = test_config(HistoryMode::Disabled);

    let first = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &first,
            started,
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Continue,
    );
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    let second = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &second,
            started + Duration::from_millis(1),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Consumed,
    );
    assert_eq!(vim.mode, crate::vim::VimMode::Normal);
    assert!(textarea.is_empty());
}

#[test]
fn pending_insert_escape_flushes_before_submit_and_paste_ownership() {
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();
    let sequence = VimInsertEscapeSequence::parse("jj").unwrap();

    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    let mut config = test_config(HistoryMode::Disabled);
    let shared = Arc::new(Mutex::new(config.clone()));
    let mut vim = VimState::with_insert_escape(true, Some(sequence.clone()));
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &enter,
            started + Duration::from_millis(1),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Continue,
    );
    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim,
        &theme,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
    ));
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "j"
    ));

    let mut paste_state = test_state().0;
    let paste_config = test_config(HistoryMode::Disabled);
    let mut paste_vim = VimState::with_insert_escape(true, Some(sequence));
    paste_vim.mode = crate::vim::VimMode::Insert;
    let mut paste_area = TextArea::default();
    paste_vim.handle_at(vim_insert_input('j'), &mut paste_area, &theme, started);
    assert!(flush_pending_insert_escape_before_non_key(
        &mut paste_vim,
        &mut paste_area,
        &mut paste_state,
        &paste_config,
    ));
    assert!(handle_paste_event(
        &Event::Paste("jj".to_string()),
        &mut paste_state,
        &paste_config,
        &action_tx,
        &mut paste_area,
    ));
    assert_eq!(textarea_text(&paste_area), "jjj");
}

#[test]
fn running_escape_exits_vim_insert_before_interrupting() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let mut config = test_config(HistoryMode::Disabled);
    config.vim_mode = true;
    config.vim_insert_escape = Some(VimInsertEscapeSequence::parse("jj").unwrap());
    let shared = Arc::new(Mutex::new(config.clone()));
    let preloaded = Arc::new(Mutex::new(None));
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();
    let mut vim = VimState::with_insert_escape(true, config.vim_insert_escape.clone());
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);
    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let event = Event::Key(key);

    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &event,
            started + Duration::from_millis(1),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Continue,
    );
    handle_status_key(
        &event,
        &key,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();

    assert_eq!(textarea_text(&textarea), "j");
    assert_eq!(state.status, AppStatus::Running);
    assert_eq!(vim.mode, crate::vim::VimMode::Normal);
    assert!(action_rx.try_recv().is_err());
}

#[test]
fn expired_insert_escape_flush_refreshes_input_state_once() {
    let theme = Theme::named(ThemeName::Dark);
    let config = test_config(HistoryMode::Disabled);
    let started = Instant::now();
    let mut vim =
        VimState::with_insert_escape(true, Some(VimInsertEscapeSequence::parse("jj").unwrap()));
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    let mut state = test_state().0;
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    assert!(flush_expired_insert_escape(
        started + Duration::from_millis(501),
        &mut vim,
        &mut textarea,
        &mut state,
        &config,
    ));
    assert_eq!(textarea_text(&textarea), "j");
    assert!(!vim.has_pending_insert_escape_for_test());
}

#[test]
fn receive_input_batch_reports_timeout_and_disconnect() {
    let (sender, receiver) = mpsc::bounded(1);
    assert_eq!(
        receive_input_batch(&receiver, Duration::from_millis(1), 64),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    drop(sender);
    assert_eq!(
        receive_input_batch(&receiver, Duration::from_millis(1), 64),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
}

#[test]
fn receive_input_or_control_prioritizes_suspend_over_queued_keys() {
    let (event_tx, event_rx) = mpsc::bounded(1);
    event_tx
        .send(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("event receiver alive");
    let (control_tx, control_rx) = mpsc::bounded(1);
    let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
    control_tx
        .send(InputControl::Suspend { acknowledge })
        .expect("control receiver alive");

    let wake = receive_input_or_control(&event_rx, &control_rx, Duration::from_millis(10), 64)
        .expect("suspend control should win");
    let InputWake::Suspend { acknowledge } = wake else {
        panic!("expected suspend control");
    };
    acknowledge
        .send(())
        .expect("acknowledgement receiver alive");
    assert_eq!(
        acknowledged.blocking_recv(),
        Ok(()),
        "input owner receives the frame-loop acknowledgement"
    );
    assert_eq!(event_rx.len(), 1, "queued key waits until resume");
}

#[test]
fn receive_input_or_control_prioritizes_focus_beyond_the_ordinary_input_cap() {
    for focus in [Event::FocusLost, Event::FocusGained] {
        let (event_tx, event_rx) = mpsc::bounded(128);
        for _ in 0..65 {
            event_tx
                .send(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                )))
                .expect("event receiver alive");
        }
        let (focus_tx, focus_rx) = mpsc::bounded(8);
        focus_tx.send(focus.clone()).expect("focus receiver alive");
        let (_control_tx, control_rx) = mpsc::bounded(1);

        let wake = receive_prioritized_input_or_control(
            &event_rx,
            &focus_rx,
            &control_rx,
            Duration::from_millis(10),
            64,
        )
        .expect("queued input should be received");
        let InputWake::Events(events) = wake else {
            panic!("expected input events");
        };

        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        let mut handled_focus = false;
        let mut handled_keys = 0;
        run_event_loop_iteration(
            &mut scheduler,
            events,
            std::iter::empty::<()>(),
            usize::MAX,
            0,
            || started,
            |event| {
                if let IterationEvent::Input(event) = event {
                    match event {
                        Event::FocusLost | Event::FocusGained => handled_focus = true,
                        Event::Key(_) => handled_keys += 1,
                        _ => {}
                    }
                }
                Ok::<Option<i32>, ()>(None)
            },
        )
        .expect("prioritized iteration");

        assert!(
            handled_focus,
            "focus changes must bypass the ordinary input cap"
        );
        assert_eq!(handled_keys, 64);
        assert_eq!(event_rx.len(), 1, "ordinary overflow remains queued");

        let next = receive_prioritized_input_or_control(
            &event_rx,
            &focus_rx,
            &control_rx,
            Duration::ZERO,
            64,
        )
        .expect("queued overflow should be returned without waiting");
        let InputWake::Events(next) = next else {
            panic!("expected queued overflow");
        };
        assert!(matches!(next.as_slice(), [Event::Key(_)]));
        assert!(event_rx.is_empty());
    }
}

#[test]
fn prioritized_focus_preserves_bounded_ordinary_input_backpressure() {
    let (event_tx, event_rx) = mpsc::bounded(128);
    let (focus_tx, focus_rx) = mpsc::bounded(8);
    let (_control_tx, control_rx) = mpsc::bounded(1);

    for _ in 0..3 {
        while event_tx.len() < 128 {
            event_tx
                .send(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                )))
                .expect("event receiver alive");
        }
        focus_tx
            .send(Event::FocusLost)
            .expect("focus receiver alive");
        let wake = receive_prioritized_input_or_control(
            &event_rx,
            &focus_rx,
            &control_rx,
            Duration::ZERO,
            64,
        )
        .expect("queued input should be received");
        assert!(matches!(wake, InputWake::Events(_)));
    }

    assert_eq!(event_rx.len(), 64);
    assert!(focus_rx.is_empty());
}

#[test]
fn clear_terminal_runs_move_all_purge_then_frame_clear() {
    let mut calls = Vec::new();

    clear_terminal_scrollback_with(
        &mut calls,
        |calls| {
            calls.push("MoveTo");
            Ok(())
        },
        |calls| {
            calls.push("All");
            Ok(())
        },
        |calls| {
            calls.push("Purge");
            Ok(())
        },
        |calls| {
            calls.push("FrameClear");
            Ok(())
        },
    )
    .expect("clear sequence should succeed");

    assert_eq!(calls, ["MoveTo", "All", "Purge", "FrameClear"]);
}

#[test]
fn clear_terminal_preserves_each_stage_error_and_short_circuits() {
    let stages = ["MoveTo", "All", "Purge", "FrameClear"];
    let kinds = [
        io::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::TimedOut,
    ];
    let messages = ["move failed", "all failed", "purge failed", "frame failed"];

    for failing_stage in 0..stages.len() {
        let mut calls = Vec::new();
        let result = clear_terminal_scrollback_with(
            &mut calls,
            |calls| {
                calls.push("MoveTo");
                if failing_stage == 0 {
                    Err(io::Error::new(kinds[0], messages[0]))
                } else {
                    Ok(())
                }
            },
            |calls| {
                calls.push("All");
                if failing_stage == 1 {
                    Err(io::Error::new(kinds[1], messages[1]))
                } else {
                    Ok(())
                }
            },
            |calls| {
                calls.push("Purge");
                if failing_stage == 2 {
                    Err(io::Error::new(kinds[2], messages[2]))
                } else {
                    Ok(())
                }
            },
            |calls| {
                calls.push("FrameClear");
                if failing_stage == 3 {
                    Err(io::Error::new(kinds[3], messages[3]))
                } else {
                    Ok(())
                }
            },
        );

        let error = result.expect_err("selected clear stage should fail");
        assert_eq!(error.kind(), kinds[failing_stage]);
        assert_eq!(error.to_string(), messages[failing_stage]);
        assert_eq!(calls, stages[..=failing_stage]);
    }
}

fn test_config(history_mode: HistoryMode) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: Some("sk-test".to_string()),
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: Default::default(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: Default::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::Dark,
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}

#[cfg(unix)]
fn stdio_mcp_server(
    name: &str,
    script: &std::path::Path,
    pid_file: &std::path::Path,
) -> orca_core::mcp_types::McpServerConfig {
    orca_core::mcp_types::McpServerConfig {
        name: name.to_string(),
        command: Some("/bin/sh".to_string()),
        args: vec![
            script.to_string_lossy().into_owned(),
            pid_file.to_string_lossy().into_owned(),
        ],
        startup_timeout_ms: Some(2_000),
        tool_timeout_ms: Some(2_000),
        ..Default::default()
    }
}

#[cfg(unix)]
fn wait_for_mcp_pid(pid_file: &std::path::Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(pid) = std::fs::read_to_string(pid_file) {
            let pid = pid.trim().to_string();
            if !pid.is_empty() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "MCP fixture did not record a process id at {}",
            pid_file.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn mcp_process_is_alive(pid: &str) -> std::io::Result<bool> {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
}

#[cfg(unix)]
fn wait_for_mcp_process_exit(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while mcp_process_is_alive(pid).expect("probe MCP fixture process") {
        assert!(
            Instant::now() < deadline,
            "MCP fixture process {pid} was not reaped before the deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn runtime_replaces_root_mcp_registry_and_reaps_replaced_stdio_process() {
    with_orca_home(|_| {
        let fixture = tempdir().expect("MCP fixture directory");
        let script = fixture.path().join("lifecycle_mcp.sh");
        let first_pid_file = fixture.path().join("first.pid");
        let second_pid_file = fixture.path().join("second.pid");
        std::fs::write(
                &script,
                r#"marker=$1
printf '%s\n' "$$" > "$marker"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"lifecycle","version":"1"}}}\n'
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"heartbeat","description":"lifecycle fixture","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
  esac
done
"#,
            )
            .expect("write MCP fixture");
        let mut first_config = test_config(HistoryMode::Record);
        first_config.mcp_servers =
            vec![stdio_mcp_server("first-session", &script, &first_pid_file)];
        let config = Arc::new(Mutex::new(first_config));
        let preloaded = Arc::new(Mutex::new(None));
        let event_tx = mpsc::unbounded().0;
        let pending = test_pending_workflow_notifications();
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host_handle = host.handle();
        let mut thread = None;

        ensure_hosted_thread(
            &mut thread,
            &host_handle,
            &config.lock().unwrap().clone(),
            &preloaded,
            "First MCP session",
            &event_tx,
        )
        .expect("first hosted session");

        let first_pid = wait_for_mcp_pid(&first_pid_file);
        let first_registry = thread
            .as_ref()
            .expect("first runtime thread")
            .mcp_registry();
        assert!(
            first_registry.errors().is_empty(),
            "runtime must construct the first session registry from its RunConfig: {:?}",
            first_registry.errors()
        );
        assert!(
            first_registry
                .tools()
                .iter()
                .any(|tool| tool.server == "first_session" && tool.name == "heartbeat"),
            "runtime must expose the first session's MCP tools"
        );
        drop(first_registry);

        config.lock().unwrap().mcp_servers =
            vec![stdio_mcp_server("next-session", &script, &second_pid_file)];
        start_new_hosted_session(&mut thread, &host_handle, &config, &preloaded, &pending)
            .expect("replacement hosted session");

        let second_pid = wait_for_mcp_pid(&second_pid_file);
        let next_registry = thread
            .as_ref()
            .expect("replacement runtime thread")
            .mcp_registry();
        assert!(
            next_registry.errors().is_empty(),
            "replacement registry must initialize cleanly: {:?}",
            next_registry.errors()
        );
        assert!(
            next_registry
                .tools()
                .iter()
                .any(|tool| tool.server == "next_session" && tool.name == "heartbeat"),
            "runtime must construct a replacement registry from the new session's RunConfig"
        );
        assert!(
            !next_registry
                .tools()
                .iter()
                .any(|tool| tool.server == "first_session"),
            "the replacement session must not retain the previous registry"
        );
        drop(next_registry);

        wait_for_mcp_process_exit(&first_pid);

        thread
            .take()
            .expect("replacement runtime thread")
            .shutdown()
            .expect("replacement thread shutdown");
        wait_for_mcp_process_exit(&second_pid);
        host.shutdown().expect("runtime host shutdown");
    });
}

fn test_state() -> (AppState, mpsc::Receiver<UserAction>) {
    let (tx, rx) = mpsc::unbounded();
    (
        AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        ),
        rx,
    )
}

fn recovery_projection_for_test(
    operation_id: orca_runtime::surface::SurfaceOperationId,
) -> SurfaceProjectionState {
    SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(1),
        session_id: Some("recovery-projection-session".to_string()),
        title: "Recovery projection".to_string(),
        usage_revision: 1,
        usage: orca_core::cost_types::UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: None,
        foreground_operation_id: Some(operation_id.clone()),
        recoverable_operation_id: Some(operation_id),
        goal_presentation: None,
        session_presentation: None,
    }
}

#[test]
fn syntax_workspace_root_preserves_real_configured_path() {
    let directory = tempdir().expect("syntax workspace");
    let mut config = test_config(HistoryMode::Disabled);
    config.cwd = Some(directory.path().to_path_buf());

    assert_eq!(
        syntax_workspace_root(&config),
        directory.path().to_path_buf()
    );
}

#[test]
fn mention_search_roots_reuse_captured_workspace_fallback() {
    let directory = tempdir().expect("captured mention workspace");
    let mut config = test_config(HistoryMode::Disabled);
    config.cwd = None;

    assert_eq!(
        mention_search_roots(&config, directory.path()),
        vec![directory.path().to_path_buf()]
    );
}

#[test]
fn startup_configures_exact_workspace_before_replay_without_starting_runtime() {
    let directory = tempdir().expect("startup syntax workspace");
    let theme = Theme::named(ThemeName::Light);
    let (mut state, _rx) = test_state();
    let historical = ChatMessage::ToolCall {
        id: "historical-edit".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
        status: "completed".to_string(),
        output: None,
        diff: Some("--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +1 @@\n-old\n+new\n".to_string()),
        kind: None,
        expanded: false,
    };

    configure_and_preload_tui_state(
        &mut state,
        directory.path().to_path_buf(),
        theme.syntax_theme,
        theme.color_level,
        [historical],
    );

    assert_eq!(
        state.syntax_workspace_root_for_test(),
        Some(directory.path())
    );
    assert_eq!(
        state.syntax_theme_for_test(),
        crate::syntax_highlight::SyntaxTheme::OneHalfLight
    );
    assert_eq!(
        state.syntax_color_level_for_test(),
        crate::terminal_capabilities::TerminalColorLevel::TrueColor
    );
    assert_eq!(state.transcript.messages.len(), 1);
    assert!(!state.edit_highlight_runtime_started_for_test());
    assert_eq!(state.pending_edit_highlight_count_for_test(), 0);
}

#[test]
fn startup_configuration_reuses_captured_workspace_after_cwd_changes() {
    struct CurrentDirGuard(PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    let _lock = crate::test_support::lock_process_env();
    let original = std::env::current_dir().expect("original current directory");
    let workspace_a = tempdir().expect("workspace A");
    let workspace_b = tempdir().expect("workspace B");
    let _restore = CurrentDirGuard(original);
    std::env::set_current_dir(workspace_a.path()).expect("set workspace A");
    let mut config = test_config(HistoryMode::Disabled);
    config.cwd = None;
    let captured_workspace = syntax_workspace_root(&config);
    std::env::set_current_dir(workspace_b.path()).expect("set workspace B");
    let theme = Theme::named(ThemeName::Catppuccin);
    let (mut state, _rx) = test_state();

    configure_tui_syntax_state(
        &mut state,
        captured_workspace.clone(),
        theme.syntax_theme,
        theme.color_level,
    );

    assert_eq!(
        state.syntax_workspace_root_for_test(),
        Some(captured_workspace.as_path())
    );
    assert_eq!(
        state.syntax_theme_for_test(),
        crate::syntax_highlight::SyntaxTheme::CatppuccinMocha
    );
}

#[test]
fn syntax_workspace_root_uses_current_dir_when_config_cwd_is_none() {
    struct CurrentDirGuard(PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    let _lock = crate::test_support::lock_process_env();
    let original = std::env::current_dir().expect("original current directory");
    let directory = tempdir().expect("fallback syntax workspace");
    let _restore = CurrentDirGuard(original);
    std::env::set_current_dir(directory.path()).expect("set fallback current directory");
    let mut config = test_config(HistoryMode::Disabled);
    config.cwd = None;
    let theme = Theme::named(ThemeName::Dark);
    let (mut state, _rx) = test_state();
    let expected_workspace = syntax_workspace_root(&config);

    assert_eq!(
        expected_workspace
            .canonicalize()
            .expect("canonical captured workspace"),
        directory
            .path()
            .canonicalize()
            .expect("canonical fallback workspace")
    );
    configure_tui_syntax_state(
        &mut state,
        expected_workspace.clone(),
        theme.syntax_theme,
        theme.color_level,
    );
    assert_eq!(
        state.syntax_workspace_root_for_test(),
        Some(expected_workspace.as_path())
    );
}

fn test_pending_workflow_notifications() -> bridge::PendingWorkflowNotifications {
    bridge::PendingWorkflowNotifications::new()
}

fn test_task_surface() -> (
    orca_runtime::runtime_host::RuntimeHost,
    RuntimeThreadHandle,
    TuiSurfaceActions,
) {
    let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
    let thread = host
        .handle()
        .start_thread(test_config(HistoryMode::Disabled), "task surface test")
        .expect("runtime thread");
    let actions = TuiSurfaceActions::new(thread.typed_surface());
    (host, thread, actions)
}

#[test]
fn runtime_ready_emits_only_attachment_and_snapshot_projection() {
    with_orca_home(|home| {
        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(home.to_path_buf());
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .handle()
            .start_thread(config, "runtime-ready projection")
            .expect("runtime thread");
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();

        announce_runtime_ready(&thread, &event_tx, &control);

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 3, "runtime-ready events: {events:?}");
        assert!(
            matches!(events[0], TuiEvent::MentionRuntimeReady(_)),
            "first runtime-ready event: {:?}",
            events[0]
        );
        assert!(
            matches!(events[1], TuiEvent::PromptQueueUpdated(_)),
            "second runtime-ready event: {:?}",
            events[1]
        );
        assert!(
            matches!(events[2], TuiEvent::SurfaceProjectionSynced(_)),
            "third runtime-ready event: {:?}",
            events[2]
        );

        thread.shutdown().expect("runtime thread shutdown");
        control.shutdown();
        host.shutdown().expect("runtime host shutdown");
    });
}

#[test]
fn hosted_tui_saved_workflow_routes_through_runtime_host() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    with_orca_home(|home| {
        let temp = tempdir().expect("workflow workspace");
        let workflow_dir = temp.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).expect("workflow directory");
        std::fs::write(
            workflow_dir.join("runtime-owned.js"),
            "export const meta = { name: 'runtime-owned', description: 'Runtime host test', phases: ['main'] };\nexport default await phase('main', async () => agent('inspect repo'));",
        )
        .expect("saved workflow");
        orca_core::config::folder_trust::set_trust_with_config_dir(
            temp.path(),
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .expect("trust workflow workspace");

        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(temp.path().to_path_buf());
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    CancelToken::new(),
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::RunWorkflow {
                name: "runtime-owned".to_string(),
                args: None,
            })
            .expect("run saved workflow action");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut events = Vec::new();
        while Instant::now() < deadline
            && !events
                .iter()
                .any(|event| matches!(event, TuiEvent::WorkflowNotification { .. }))
        {
            if let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
                events.push(event);
            }
        }
        while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(100)) {
            events.push(event);
        }
        let action_completed_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    TuiEvent::SessionCompleted { status } if status == "success"
                )
            })
            .expect("workflow slash action should complete after durable launch");
        let workflow_completed_at = events
            .iter()
            .position(|event| matches!(event, TuiEvent::WorkflowNotification { .. }))
            .expect("workflow should publish a terminal notification");
        assert!(
            action_completed_at < workflow_completed_at,
            "slash action completion must precede background workflow terminal: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection
                        .workflow_tasks
                        .iter()
                        .any(|task| task.name.as_deref() == Some("runtime-owned"))
            )),
            "saved workflow should publish a typed task projection: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TuiEvent::WorkflowNotification { status, .. } if status == "completed")),
            "saved workflow should publish a terminal notification"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TuiEvent::WorkflowNotification { .. }))
                .count(),
            1,
            "one workflow run must produce exactly one terminal notification: {events:?}"
        );
        action_tx
            .send(UserAction::Cancel)
            .expect("stop TUI test loop");
        handle.join().expect("hosted TUI test loop joined");
    });
}

#[test]
fn typed_workflow_launch_rejects_disabled_history_without_waiting_for_surface_readiness() {
    let (host, _thread, actions) = test_task_surface();
    let (event_tx, _event_rx) = mpsc::unbounded();
    let started = Instant::now();
    let error = actions
        .launch_workflow("missing", None, &event_tx)
        .expect_err("durable workflow must require a recorded session");
    assert!(error.contains("requires recorded conversation history"));
    assert!(started.elapsed() < Duration::from_secs(1));
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn hosted_tui_failed_workflow_launch_rejects_the_foreground_action() {
    with_orca_home(|home| {
        let temp = tempdir().expect("workflow workspace");
        orca_core::config::folder_trust::set_trust_with_config_dir(
            temp.path(),
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .expect("trust workflow workspace");

        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(temp.path().to_path_buf());
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    CancelToken::new(),
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::RunWorkflow {
                name: "missing-workflow".to_string(),
                args: None,
            })
            .expect("run missing saved workflow action");
        let rejected = loop {
            match event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(TuiEvent::OperationRejected(error)) => break error,
                Ok(_) => {}
                Err(error) => panic!("workflow launch rejection missing: {error}"),
            }
        };
        assert!(rejected.contains("typed TUI workflow launch failed"));

        action_tx
            .send(UserAction::Cancel)
            .expect("stop TUI test loop");
        handle.join().expect("hosted TUI test loop joined");
    });
}

#[test]
fn resumed_tui_hydrates_durable_workflow_tasks_from_surface_snapshot() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    with_orca_home(|home| {
        let temp = tempdir().expect("workflow workspace");
        let workflow_dir = temp.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).expect("workflow directory");
        std::fs::write(
                workflow_dir.join("restart-visible.js"),
                "export const meta = { name: 'restart-visible', description: 'Restart visible', phases: ['main'] };\nexport default await phase('main', async () => agent('inspect repo'));",
            )
            .expect("saved workflow");
        orca_core::config::folder_trust::set_trust_with_config_dir(
            temp.path(),
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .expect("trust workflow workspace");

        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(temp.path().to_path_buf());
        config.output_format = OutputFormat::Jsonl;
        config.approval_mode = ApprovalMode::FullAuto;
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .handle()
            .start_thread(config.clone(), "restart workflow projection")
            .expect("runtime thread");
        let session_id = thread.session_id().expect("recorded session").to_string();
        let (workflow_tx, workflow_rx) = mpsc::unbounded();
        TuiSurfaceActions::new(thread.typed_surface())
            .launch_workflow("restart-visible", None, &workflow_tx)
            .expect("typed workflow launch");
        loop {
            match workflow_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(TuiEvent::WorkflowNotification { status, .. }) => {
                    assert_eq!(status, "completed");
                    break;
                }
                Ok(TuiEvent::Error(error)) => panic!("typed workflow failed: {error}"),
                Ok(_) => {}
                Err(error) => panic!("typed workflow terminal event missing: {error}"),
            }
        }
        host.shutdown().expect("shutdown original runtime");

        let transcript =
            orca_runtime::history::load_session(&session_id).expect("saved transcript");
        let mut resumed_config = test_config(HistoryMode::Resume(session_id.clone()));
        resumed_config.cwd = Some(temp.path().to_path_buf());
        resumed_config.output_format = OutputFormat::Jsonl;
        resumed_config.approval_mode = ApprovalMode::FullAuto;
        let resumed_host =
            orca_runtime::runtime_host::RuntimeHost::start().expect("resumed runtime host");
        let resumed = resumed_host
            .handle()
            .start_thread_with_request(
                RuntimeThreadStartRequest::new(resumed_config, "resume workflow projection")
                    .with_preloaded(transcript),
            )
            .expect("resumed runtime thread");
        let (event_tx, event_rx) = mpsc::unbounded();
        emit_typed_history_snapshot(&resumed, &HistoryMode::Resume(session_id), None, &event_tx)
            .expect("typed restart snapshot");
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            TuiEvent::SurfaceProjectionSynced(projection)
                if projection.workflow_tasks.iter().any(|task| {
                    task.name.as_deref() == Some("restart-visible")
                        && task.status == orca_core::task_types::TaskStatus::Completed
                })
        )));
        resumed_host.shutdown().expect("shutdown resumed runtime");
    });
}

fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
    TuiInteractionKey::new(
        orca_core::cancel::OperationIdAllocator::new().allocate(),
        id,
        kind,
    )
}

#[test]
fn user_submission_error_emits_rejection_terminal() {
    let (event_tx, event_rx) = mpsc::unbounded();

    send_submission_error(
        &event_tx,
        None,
        Some("review @gone.txt"),
        "bound file is no longer available".to_string(),
    );

    assert!(matches!(
        event_rx.try_recv(),
        Ok(TuiEvent::SubmissionRejected {
            prompt, message, ..
        })
            if prompt == "review @gone.txt"
                && message == "bound file is no longer available"
    ));
}

#[test]
fn terminal_recovery_error_does_not_fabricate_failure_terminal() {
    let (event_tx, event_rx) = mpsc::unbounded();
    let error = crate::surface_client::terminal_recovery_error_for_test(
        "terminal commit requires recovery",
    );

    emit_hosted_operation_error(&event_tx, error, &HostedOperationKind::Turn);

    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        matches!(events.as_slice(), [TuiEvent::Error(message)] if message.contains("requires recovery"))
    );
}

#[test]
fn stale_bound_file_preparation_emits_submission_rejected() {
    with_orca_home(|_| {
        let root = tempdir().expect("workspace root");
        let root_path = root
            .path()
            .canonicalize()
            .expect("canonical workspace root");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = Some(root_path.clone());
        config.runtime_workspace_roots = Some(vec![root_path.clone()]);
        let prompt = "review @gone.txt";
        let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: 7,
                end: prompt.len(),
                visible: "@gone.txt".to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: root_path,
                    path: "gone.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );
        let mut harness = HostedTuiHarness::start(config, None);

        harness.send(UserAction::SubmitWithMentions {
            prompt: prompt.to_string(),
            bindings,
            images: Vec::new(),
        });

        let rejection =
            harness.recv_until(|event| matches!(event, TuiEvent::SubmissionRejected { .. }));
        assert!(matches!(
            rejection,
            TuiEvent::SubmissionRejected {
                prompt, message, ..
            }
                if prompt == "review @gone.txt"
                    && message.contains("failed to resolve bound @gone.txt")
        ));
        harness.shutdown();
    });
}

#[test]
fn queued_stale_bound_file_rejection_preserves_queued_identity() {
    with_orca_home(|_| {
        let root = tempdir().expect("workspace root");
        let root_path = root
            .path()
            .canonicalize()
            .expect("canonical workspace root");
        let mut config = test_config(HistoryMode::Disabled);
        config.cwd = Some(root_path.clone());
        config.runtime_workspace_roots = Some(vec![root_path.clone()]);
        let prompt = "review @gone.txt";
        let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: 7,
                end: prompt.len(),
                visible: "@gone.txt".to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: root_path,
                    path: "gone.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );
        let mut harness = HostedTuiHarness::start(config, None);

        harness.send(UserAction::SubmitQueued {
            id: 42,
            prompt: prompt.to_string(),
            bindings,
            images: Vec::new(),
        });

        let rejection =
            harness.recv_until(|event| matches!(event, TuiEvent::SubmissionRejected { .. }));
        assert!(matches!(
            rejection,
            TuiEvent::SubmissionRejected {
                queued_id: Some(42),
                prompt,
                message,
                ..
            } if prompt == "review @gone.txt"
                && message.contains("failed to resolve bound @gone.txt")
        ));
        harness.shutdown();
    });
}

#[test]
fn workflow_submission_error_remains_generic() {
    let (event_tx, event_rx) = mpsc::unbounded();

    send_submission_error(&event_tx, None, None, "workflow failed".to_string());

    assert!(matches!(
        event_rx.try_recv(),
        Ok(TuiEvent::Error(message)) if message == "workflow failed"
    ));
}

#[test]
fn esc_clears_mouse_selection_before_other_esc_semantics() {
    let (mut state, _rx) = test_state();
    let config = test_config(HistoryMode::Record);
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut vim = VimState::new(false);

    let pos = crate::selection::SelectionPos { row: 0, col: 0 };
    let head = crate::selection::SelectionPos { row: 2, col: 5 };
    state.viewport.selection = Some(crate::selection::TranscriptSelection {
        anchor: pos,
        head,
        dragging: false,
        granularity: crate::selection::SelectionGranularity::Cell,
        origin: (pos, head),
    });

    let flow = handle_key_event_preflight(
        crossterm::event::KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        &mut state,
        &config,
        &action_tx,
        &mut vim,
        false,
        || Ok(()),
    )
    .expect("preflight");

    assert!(matches!(flow, KeyEventFlow::Continue));
    assert_eq!(state.viewport.selection, None);

    // Without a selection, Esc falls through to its usual handling.
    let flow = handle_key_event_preflight(
        crossterm::event::KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        &mut state,
        &config,
        &action_tx,
        &mut vim,
        false,
        || Ok(()),
    )
    .expect("preflight");
    assert!(matches!(flow, KeyEventFlow::Unhandled));
}

#[test]
fn manual_compaction_emits_started_before_running_summary_work() {
    let (event_tx, event_rx) = mpsc::unbounded();

    run_manual_compaction_with_events(&event_tx, || {
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::CompactionStarted)
        ));
        (12, 5)
    });

    assert!(matches!(
        event_rx.try_recv(),
        Ok(TuiEvent::Compacted {
            before_messages: 12,
            after_messages: 5,
            ..
        })
    ));
}

#[test]
fn manual_compaction_starts_with_a_fresh_cancel_state() {
    let (event_tx, _event_rx) = mpsc::unbounded();
    let previous = CancelToken::new();
    previous.cancel();
    assert!(previous.is_cancelled());
    let current = CancelToken::new();

    run_manual_compaction_with_events(&event_tx, || {
        assert!(
            !current.is_cancelled(),
            "a prior turn interrupt must not cancel the next manual compaction"
        );
        (8, 3)
    });
}

fn matching_task_update(
    event: TuiEvent,
    predicate: impl Fn(&orca_core::task_types::BackgroundTaskSummary) -> bool,
) -> Option<orca_core::task_types::BackgroundTaskSummary> {
    match event {
        TuiEvent::SurfaceProjectionSynced(projection) => {
            projection.workflow_tasks.into_iter().find(predicate)
        }
        _ => None,
    }
}

fn workflow_task(id: &str, name: &str) -> orca_core::task_types::BackgroundTaskSummary {
    orca_core::task_types::BackgroundTaskSummary {
        id: id.to_string(),
        task_type: orca_core::task_types::TaskType::Workflow,
        status: orca_core::task_types::TaskStatus::Running,
        is_backgrounded: false,
        description: name.to_string(),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: None,
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        pending_tool_call: None,
        name: Some(name.to_string()),
        workflow_run_id: Some(format!("run-{id}")),
        phase_count: Some(1),
        workflow_progress: None,
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        workflow_script_path: None,
        workflow_launch_input: None,
        workflow_final_summary: None,
        workflow_failure_count: 0,
        usage: None,
        subagent_current_activity: None,
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
fn stale_attachment_events_do_not_mutate_switched_session() {
    let (mut state, _rx) = test_state();
    let attachment_a = SessionAttachmentId::new(1);
    let attachment_b = SessionAttachmentId::new(2);
    let apply = |state: &mut AppState, attachment, event| {
        reduce_attached_tui_event(
            state,
            AttachedTuiEvent {
                attachment: Some(attachment),
                event,
            },
        )
    };

    assert!(apply(
        &mut state,
        attachment_a,
        TuiEvent::SessionAttachmentActivated,
    ));
    assert!(apply(
        &mut state,
        attachment_b,
        TuiEvent::SessionAttachmentActivated,
    ));

    let goal_b = orca_core::goal_types::ThreadGoal {
        session_id: "session-b".to_string(),
        objective: "goal-b".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: Some(1_000),
        tokens_used: 10,
        time_used_seconds: 2,
        created_at: 1,
        updated_at: 2,
    };
    let operation_b = orca_runtime::surface::SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 0x0b,
    ])
    .unwrap();
    for event in [
        TuiEvent::HistoryLoaded {
            messages: vec![ChatMessage::Assistant("transcript-b".to_string())],
            plan: None,
            label: "loaded-b".to_string(),
        },
        TuiEvent::SurfaceProjectionSynced(Box::new(SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(20),
            session_id: Some("session-b".to_string()),
            title: "title-b".to_string(),
            usage_revision: 20,
            usage: orca_core::cost_types::UsageTotals {
                input_tokens: 200,
                output_tokens: 50,
                cache_tokens: 0,
                estimated_cost_usd: 0.25,
            },
            context_revision: 1,
            context_used_tokens: 200,
            context_limit_tokens: 128_000,
            workflow_tasks: vec![workflow_task("task-b", "workflow-b")],
            current_goal: Some(goal_b.clone()),
            foreground_operation_id: Some(operation_b.clone()),
            recoverable_operation_id: Some(operation_b.clone()),
            goal_presentation: None,
            session_presentation: None,
        })),
        TuiEvent::TurnStarted {
            turn: 3,
            task: None,
        },
    ] {
        assert!(apply(&mut state, attachment_b, event));
    }

    let messages = format!("{:?}", state.transcript.messages);
    let tasks = state.workflow_tasks().to_vec();
    let usage = state.usage().clone();
    let goal = state.current_goal().cloned();
    let identity = (
        state.current_session_id().map(ToOwned::to_owned),
        state.current_session_title().map(ToOwned::to_owned),
    );
    let status = state.status;
    let recovery = state.recoverable_operation_id().cloned();

    let goal_a = orca_core::goal_types::ThreadGoal {
        session_id: "session-a".to_string(),
        objective: "stale-goal-a".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Complete,
        token_budget: None,
        tokens_used: 999,
        time_used_seconds: 99,
        created_at: 1,
        updated_at: 99,
    };
    for event in [
        TuiEvent::HistoryLoaded {
            messages: vec![ChatMessage::Assistant("stale-transcript-a".to_string())],
            plan: None,
            label: "stale-loaded-a".to_string(),
        },
        TuiEvent::SurfaceProjectionSynced(Box::new(SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(99),
            session_id: Some("session-a".to_string()),
            title: "stale-title-a".to_string(),
            usage_revision: 99,
            usage: orca_core::cost_types::UsageTotals {
                input_tokens: 9_999,
                output_tokens: 9_999,
                cache_tokens: 0,
                estimated_cost_usd: 99.0,
            },
            context_revision: 99,
            context_used_tokens: 9_999,
            context_limit_tokens: 128_000,
            workflow_tasks: vec![workflow_task("task-a", "stale-workflow-a")],
            current_goal: Some(goal_a.clone()),
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        })),
        TuiEvent::MessageDelta("stale-delta-a".to_string()),
        TuiEvent::SessionCompleted {
            status: "failed".to_string(),
        },
    ] {
        assert!(!apply(&mut state, attachment_a, event));
    }

    assert_eq!(format!("{:?}", state.transcript.messages), messages);
    assert_eq!(state.workflow_tasks(), tasks);
    assert_eq!(state.usage(), &usage);
    assert_eq!(state.current_goal(), goal.as_ref());
    assert_eq!(
        (
            state.current_session_id().map(ToOwned::to_owned),
            state.current_session_title().map(ToOwned::to_owned),
        ),
        identity
    );
    assert_eq!(state.status, status);
    assert_eq!(state.recoverable_operation_id(), recovery.as_ref());
}

#[test]
fn attachment_relay_preserves_source_generation_after_rotation() {
    let (root_event_tx, root_event_rx) = mpsc::unbounded();
    let mut attachment = SessionAttachmentId::new(1);
    let mut event_tx = spawn_attached_event_sender(root_event_tx.clone(), attachment);

    let receive_attached = || match root_event_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("attached TUI event")
    {
        TuiEvent::Attached(attached) => *attached,
        event => panic!("expected attached TUI event, got {event:?}"),
    };

    let activated_a = receive_attached();
    assert_eq!(activated_a.attachment, Some(SessionAttachmentId::new(1)));
    assert!(matches!(
        activated_a.event,
        TuiEvent::SessionAttachmentActivated
    ));

    let stale_event_tx = event_tx.clone();
    rotate_attached_event_sender(&root_event_tx, &mut attachment, &mut event_tx, None);
    let activated_b = receive_attached();
    assert_eq!(activated_b.attachment, Some(SessionAttachmentId::new(2)));
    assert!(matches!(
        activated_b.event,
        TuiEvent::SessionAttachmentActivated
    ));

    stale_event_tx
        .send(TuiEvent::MessageDelta("from-a".to_string()))
        .unwrap();
    event_tx
        .send(TuiEvent::MessageDelta("from-b".to_string()))
        .unwrap();

    let delivered = [receive_attached(), receive_attached()];
    assert!(delivered.iter().any(|attached| {
        attached.attachment == Some(SessionAttachmentId::new(1))
            && matches!(
                &attached.event,
                TuiEvent::MessageDelta(text) if text == "from-a"
            )
    }));
    assert!(delivered.iter().any(|attached| {
        attached.attachment == Some(SessionAttachmentId::new(2))
            && matches!(
                &attached.event,
                TuiEvent::MessageDelta(text) if text == "from-b"
            )
    }));
}

#[test]
fn routed_rotation_replays_hidden_parent_interaction_after_side_return() {
    let (root_event_tx, root_event_rx) = mpsc::unbounded();
    let mut parent_attachment = SessionAttachmentId::new(1);
    let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent_attachment)));
    let mut parent_event_tx = spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        parent_attachment,
        Some(routing.clone()),
    );
    AttachmentRouting::switch_attachment(&routing, &root_event_tx, parent_attachment, None, false);
    let _ = root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    rotate_attached_event_sender(
        &root_event_tx,
        &mut parent_attachment,
        &mut parent_event_tx,
        Some(&routing),
    );
    let _ = root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let side_attachment = parent_attachment.next();
    let _side_event_tx = spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        side_attachment,
        Some(routing.clone()),
    );
    AttachmentRouting::switch_attachment(
        &routing,
        &root_event_tx,
        side_attachment,
        Some(parent_attachment),
        false,
    );
    let _ = root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    parent_event_tx
        .send(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "rotated-parent"),
            tool: "bash".to_string(),
            target: None,
            preview: None,
        })
        .unwrap();
    assert!(matches!(
        root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        TuiEvent::SideParentStatusChanged(SideParentStatus::NeedsApproval)
    ));

    AttachmentRouting::switch_attachment(
        &routing,
        &root_event_tx,
        parent_attachment,
        Some(parent_attachment),
        true,
    );
    let _ = root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        root_event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        TuiEvent::Attached(attached)
            if attached.attachment == Some(parent_attachment)
                && matches!(
                    &attached.event,
                    TuiEvent::ApprovalNeeded { key, .. }
                        if key.request_id == "rotated-parent"
                )
    ));
}

#[test]
fn workflows_panel_keys_move_selected_task() {
    let (mut state, _rx) = test_state();
    state.show_workflows();
    state.replace_workflow_tasks_for_test(vec![
        workflow_task("task-1", "audit"),
        workflow_task("task-2", "repair"),
    ]);

    let action_tx = state.event_tx.clone();

    assert!(handle_workflows_panel_key(
        KeyCode::Down,
        &mut state,
        &action_tx
    ));
    assert_eq!(state.workflow_selected_index(), 1);

    assert!(handle_workflows_panel_key(
        KeyCode::Up,
        &mut state,
        &action_tx
    ));
    assert_eq!(state.workflow_selected_index(), 0);
}

#[test]
fn workflows_panel_enter_opens_selected_background_approval() {
    let (mut state, _rx) = test_state();
    let mut task = workflow_task("task-approval", "approval");
    task.task_type = orca_core::task_types::TaskType::MainSession;
    task.status = orca_core::task_types::TaskStatus::ApprovalRequired;
    task.is_backgrounded = true;
    task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
        id: "mock-tool-1".to_string(),
        name: "task_list".to_string(),
        action: orca_core::approval_types::ActionKind::Read,
        target: None,
        arguments: "{}".to_string(),
    });
    state.show_workflows();
    state.replace_workflow_tasks_for_test(vec![task]);

    let action_tx = state.event_tx.clone();
    assert!(handle_workflows_panel_key(
        KeyCode::Enter,
        &mut state,
        &action_tx
    ));

    let dialog = state.approval_dialog.as_ref().expect("approval dialog");
    assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
    assert_eq!(state.status, AppStatus::WaitingApproval);
}

#[test]
fn workflows_panel_s_key_handles_selected_running_task() {
    let (mut state, rx) = test_state();
    let mut task = workflow_task("task-running", "running");
    task.status = orca_core::task_types::TaskStatus::Running;
    state.show_workflows();
    state.replace_workflow_tasks_for_test(vec![task]);

    let action_tx = state.event_tx.clone();
    assert!(handle_workflows_panel_key(
        KeyCode::Char('s'),
        &mut state,
        &action_tx
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(UserAction::StopTask { task_id }) if task_id == "task-running"
    ));
}

#[test]
fn workflows_panel_f_key_handles_selected_backgrounded_main_session() {
    let (mut state, rx) = test_state();
    let mut task = workflow_task("task-main", "backgrounded");
    task.task_type = orca_core::task_types::TaskType::MainSession;
    task.status = orca_core::task_types::TaskStatus::Running;
    task.is_backgrounded = true;
    state.show_workflows();
    state.replace_workflow_tasks_for_test(vec![task]);

    let action_tx = state.event_tx.clone();
    assert!(handle_workflows_panel_key(
        KeyCode::Char('f'),
        &mut state,
        &action_tx
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(UserAction::ForegroundTask { task_id }) if task_id == "task-main"
    ));
}

#[test]
fn background_approval_resolution_sends_request_scoped_action() {
    let (mut state, rx) = test_state();
    let action_tx = state.event_tx.clone();
    state.approval_dialog = Some(crate::types::ApprovalDialog {
        id: "approval-background".to_string(),
        interaction: None,
        tool: "task_list".to_string(),
        target: None,
        permission_kind: None,
        background_task_id: Some("task-approval".to_string()),
        selected: 0,
        options: vec![ApprovalOption::Once, ApprovalOption::Deny],
        diff: None,
    });
    state.set_status(AppStatus::WaitingApproval);

    resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

    assert!(matches!(
        rx.try_recv(),
        Ok(UserAction::ResolveBackgroundApproval { id, approved })
            if id == "approval-background" && approved
    ));
    assert_eq!(state.status, AppStatus::Idle);
    assert!(state.approval_dialog.is_none());
}

#[test]
fn foreground_approval_resolution_sends_runtime_interaction_id() {
    let (mut state, rx) = test_state();
    let action_tx = state.event_tx.clone();
    state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval-foreground"),
        tool: "bash".to_string(),
        target: Some("cargo test".to_string()),
        preview: None,
    });

    resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

    assert!(matches!(
        rx.try_recv(),
        Ok(UserAction::RespondToInteraction {
            key,
            response: TuiInteractionResponse::Approval(true),
        }) if key.request_id == "approval-foreground"
    ));
    assert_eq!(state.status, AppStatus::Running);
    assert!(state.approval_dialog.is_none());
}

#[test]
fn registry_only_background_approval_is_not_presented_as_typed_recoverable() {
    let (host, thread, actions) = test_task_surface();
    let registry = thread.task_registry();
    let task = registry.create_main_session("Needs approval".to_string());
    registry.mark_running(&task.id).unwrap();
    registry.mark_backgrounded(&task.id).unwrap();
    registry
        .approval_required_for_pending_tool(
            &task.id,
            "approval_required".to_string(),
            Some(orca_core::task_types::PendingToolCallSummary {
                id: "mock-tool-1".to_string(),
                name: "task_list".to_string(),
                action: orca_core::approval_types::ActionKind::Read,
                target: None,
                arguments: "{}".to_string(),
            }),
        )
        .unwrap();
    let (event_tx, event_rx) = mpsc::unbounded();

    assert_eq!(
        notify_recovered_background_approvals_for_tui(&actions, &event_tx),
        0
    );
    assert!(event_rx.try_recv().is_err());
    host.shutdown().expect("runtime host shutdown");
}

#[test]
fn resumed_registry_only_approval_is_not_advertised_as_actionable() {
    with_orca_home(|home| {
        let session_id = "resume-background-approval-session";
        let registry = orca_runtime::tasks::TaskRegistry::new_persistent(
            session_id.to_string(),
            home.join("task-sessions"),
        )
        .unwrap();
        let task = registry.create_main_session("Needs approval".to_string());
        let task_id = task.id.clone();
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        drop(registry);

        let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
            session_id.to_string(),
        ))));
        let fixture = transcript(session_id);
        let mut writer = history::SessionWriter::start_from_meta(fixture.meta)
            .expect("create resumable approval transcript");
        writer.complete("approval_required").unwrap();
        let transcript =
            history::load_session(session_id).expect("load resumable approval transcript");
        let preloaded = Arc::new(Mutex::new(Some(transcript)));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit("hello".to_string()))
            .unwrap();

        let mut seen = Vec::new();
        for _ in 0..20 {
            match event_rx.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("hosted TUI event channel disconnected")
                }
                Ok(TuiEvent::Notice(message))
                    if message.contains("Recovered background session")
                        && message.contains("task_list") =>
                {
                    panic!("registry-only approval was advertised as actionable");
                }
                Ok(TuiEvent::SurfaceProjectionSynced(projection))
                    if projection
                        .workflow_tasks
                        .iter()
                        .any(|task| task.id == task_id) =>
                {
                    panic!("registry-only approval leaked into typed task projection");
                }
                Ok(TuiEvent::TurnStarted { .. }) => break,
                Ok(event) => seen.push(format!("{event:?}")),
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(
            seen.iter()
                .all(|event| !event.contains("Recovered background session")),
            "registry-only approval was advertised; saw {seen:?}"
        );
    });
}

#[test]
fn resumed_tui_projects_reconciled_terminal_legacy_task_as_non_actionable() {
    with_orca_home(|home| {
        let session_id = "019c6f42-9a21-7e30-8c4d-5e6f708192a3";
        let fixture = transcript(session_id);
        let mut writer = history::SessionWriter::start_from_meta(fixture.meta)
            .expect("create resumable terminal-task transcript");
        writer.complete("completed").unwrap();
        let preloaded =
            history::load_session(session_id).expect("load resumable terminal-task transcript");

        let registry = orca_runtime::tasks::TaskRegistry::new_persistent(
            session_id.to_string(),
            home.join("task-sessions"),
        )
        .expect("open persistent terminal-task registry");
        let task = registry.create_main_session("completed before typed recovery".to_string());
        let task_id = task.id.clone();
        registry
            .complete(&task.id, "durable terminal result".to_string())
            .expect("complete registry-only terminal task");
        drop(registry);

        let mut harness = HostedTuiHarness::start(
            test_config(HistoryMode::Resume(session_id.to_string())),
            Some(preloaded),
        );
        let projection = match harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.workflow_tasks.iter().any(|task| task.id == task_id))
        }) {
            TuiEvent::SurfaceProjectionSynced(projection) => projection,
            _ => unreachable!(),
        };
        let projected = projection
            .workflow_tasks
            .iter()
            .filter(|task| task.id == task_id)
            .collect::<Vec<_>>();
        assert_eq!(projected.len(), 1);
        assert_eq!(
            projected[0].task_type,
            orca_core::task_types::TaskType::MainSession
        );
        assert_eq!(
            projected[0].status,
            orca_core::task_types::TaskStatus::Completed
        );
        assert!(!projected[0].is_backgrounded);
        assert!(projected[0].pending_tool_call.is_none());
        assert_eq!(
            projected[0].result.as_deref(),
            Some("durable terminal result")
        );
        let accepted_cursor = projection.cursor.clone();

        harness.send(UserAction::StopTask {
            task_id: task_id.clone(),
        });
        harness.send(UserAction::ForegroundTask {
            task_id: task_id.clone(),
        });
        let mut errors = 0;
        while errors < 2 {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("terminal task action rejection")
            {
                TuiEvent::Error(_) => errors += 1,
                TuiEvent::Notice(message)
                    if message.contains(&task_id)
                        && (message.contains("stop requested")
                            || message.contains("returned to foreground")) =>
                {
                    panic!("terminal task action fabricated success notice: {message}");
                }
                TuiEvent::SurfaceProjectionSynced(next)
                    if next.cursor != accepted_cursor
                        && next.workflow_tasks.iter().any(|task| task.id == task_id) =>
                {
                    panic!("terminal task action fabricated a new success projection");
                }
                _ => {}
            }
        }
        harness.shutdown();
    });
}

#[test]
fn background_approval_action_denial_stops_task_and_refreshes_tasks() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });
        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();
        loop {
            if matches!(
                event_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                TuiEvent::MessageDelta(text)
                    if text.contains("Mock slow tool stream started.")
            ) {
                break;
            }
        }
        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();
        let (task_id, approval_id) = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
            }) {
                break (
                    task.id,
                    task.pending_tool_call.expect("pending background tool").id,
                );
            }
        };
        action_tx
            .send(UserAction::ResolveBackgroundApproval {
                id: approval_id,
                approved: false,
            })
            .unwrap();
        let mut stopped = false;
        let mut denied_notice = false;
        let mut seen = Vec::new();
        for _ in 0..20 {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match event {
                TuiEvent::SurfaceProjectionSynced(projection) => {
                    stopped |= projection.workflow_tasks.iter().any(|task| {
                        task.id == task_id
                            && matches!(
                                task.status,
                                orca_core::task_types::TaskStatus::Stopped
                                    | orca_core::task_types::TaskStatus::Cancelled
                            )
                            && task.pending_tool_call.is_none()
                    });
                }
                TuiEvent::Notice(message) if message.contains("Background approval denied") => {
                    denied_notice = true;
                }
                event => seen.push(format!("{event:?}")),
            }
            if stopped && denied_notice {
                break;
            }
        }
        assert!(
            stopped,
            "denied background task was not stopped; saw {seen:?}"
        );
        assert!(
            denied_notice,
            "denied background approval notice was not emitted; saw {seen:?}"
        );

        action_tx.send(UserAction::Interrupt).unwrap();
        action_tx
            .send(UserAction::Submit(
                "turn after denied background approval".to_string(),
            ))
            .unwrap();
        let mut next_turn_started = false;
        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::TurnStarted { .. } => next_turn_started = true,
                TuiEvent::SessionCompleted { status } if next_turn_started => {
                    assert_eq!(status, "success");
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();
    });
}

fn transcript(session_id: &str) -> history::SessionTranscript {
    history::SessionTranscript {
        meta: history::SessionMeta {
            schema_version: 1,
            session_id: session_id.to_string(),
            cwd: "/tmp".to_string(),
            provider: "mock".to_string(),
            model: Some("auto".to_string()),
            title: "resumed goal".to_string(),
            created_at: chrono::Utc::now(),
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            metadata_writable_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        },
        messages: Vec::new(),
        compactions: Vec::new(),
        summaries: Vec::new(),
        usage: None,
        plan: None,
        completion_status: None,
        completion_error: None,
        next_event_seq: 0,
        semantic_events: Vec::new(),
        path: std::env::temp_dir().join("resumed-goal.jsonl"),
    }
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let home = crate::test_support::isolate_orca_home();
    f(home.path())
}

#[test]
fn saved_history_fallback_reports_load_failure() {
    with_orca_home(|_| {
        let error = load_saved_history_fallback("missing-session")
            .expect_err("missing session must not produce an empty resumed transcript");
        assert!(error.contains("failed to load saved conversation missing-session"));
    });
}

struct HostedTuiHarness {
    action_tx: mpsc::Sender<UserAction>,
    event_rx: mpsc::Receiver<TuiEvent>,
    runtime: TuiAgentRuntime,
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
}

impl HostedTuiHarness {
    fn start(config: RunConfig, preloaded: Option<history::SessionTranscript>) -> Self {
        Self::start_with_background_capacity(config, preloaded, 8)
    }

    fn start_with_background_capacity(
        config: RunConfig,
        preloaded: Option<history::SessionTranscript>,
        background_capacity: usize,
    ) -> Self {
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(preloaded));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let runtime = spawn_hosted_tui_test_runtime_with_background_capacity(
            Arc::clone(&config),
            Arc::clone(&preloaded),
            event_tx,
            action_rx,
            background_capacity,
        );
        Self {
            action_tx,
            event_rx,
            runtime,
            config,
            preloaded,
        }
    }

    fn send(&self, action: UserAction) {
        self.action_tx.send(action).expect("hosted TUI action");
    }

    fn recv_until(&self, mut predicate: impl FnMut(&TuiEvent) -> bool) -> TuiEvent {
        loop {
            let event = self
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("hosted TUI event");
            if predicate(&event) {
                return event;
            }
        }
    }

    fn shutdown(&mut self) {
        self.runtime.shutdown().expect("hosted TUI shutdown");
    }
}

struct AttachedHostedTuiHarness {
    action_tx: mpsc::Sender<UserAction>,
    event_rx: mpsc::Receiver<TuiEvent>,
    runtime: TuiAgentRuntime,
    state: AppState,
}

impl AttachedHostedTuiHarness {
    fn start(config: RunConfig) -> Self {
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let pending = bridge::PendingWorkflowNotifications::new();
        let controller = TuiSurfaceTaskControl::new();
        let agent_config = Arc::clone(&config);
        let agent_preloaded = Arc::clone(&preloaded);
        let agent_events = event_tx.clone();
        let agent_pending = pending.clone();
        let runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            8,
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
        .expect("attached hosted TUI runtime");
        let (state, _actions) = test_state();
        Self {
            action_tx,
            event_rx,
            runtime,
            state,
        }
    }

    fn send(&self, action: UserAction) {
        self.action_tx
            .send(action)
            .expect("attached hosted TUI action");
    }

    fn recv_until(&mut self, mut predicate: impl FnMut(&TuiEvent) -> bool) -> TuiEvent {
        loop {
            let event = self
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("attached hosted TUI event");
            let event = match accept_attached_tui_event(&mut self.state, event) {
                Ok(Some(event)) => event,
                Ok(None) | Err(()) => continue,
            };
            self.state.update(event.clone());
            if predicate(&event) {
                return event;
            }
        }
    }
}

impl Drop for AttachedHostedTuiHarness {
    fn drop(&mut self) {
        let _ = self.runtime.shutdown();
    }
}

#[test]
fn hosted_tui_submit_clears_actor_operation_before_terminal_ui_event() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let pending = test_pending_workflow_notifications();
        let (event_tx, event_rx) = mpsc::unbounded();
        let event_tx = spawn_unwrapped_tui_test_event_sender(event_tx);
        let (action_tx, action_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::new();
        let agent_config = Arc::clone(&config);
        let agent_preloaded = Arc::clone(&preloaded);
        let agent_events = event_tx.clone();
        let agent_pending = pending.clone();
        let mut runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            8,
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
        .expect("hosted TUI runtime");

        action_tx
            .send(UserAction::Submit("hello from hosted TUI".to_string()))
            .unwrap();
        loop {
            if let TuiEvent::SessionCompleted { status } =
                event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                assert_eq!(status, "success");
                assert_eq!(runtime.controller().current_id(), None);
                break;
            }
        }
        action_tx.send(UserAction::Compact).unwrap();
        let mut saw_compaction_start = false;
        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::CompactionStarted => saw_compaction_start = true,
                TuiEvent::Compacted { .. } => {
                    assert!(saw_compaction_start);
                    assert_eq!(runtime.controller().current_id(), None);
                    break;
                }
                TuiEvent::OperationRejected(message) | TuiEvent::Error(message) => {
                    panic!("manual compaction failed: {message}");
                }
                _ => {}
            }
        }
        runtime.shutdown().expect("hosted runtime shutdown");
    });
}

#[test]
fn ordinary_tui_dispatch_commits_user_and_provider_items_to_typed_surface() {
    with_orca_home(|_| {
        let config = test_config(HistoryMode::Record);
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .handle()
            .start_thread(config.clone(), "typed ordinary turn ingress")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::new();
        let control = controller.clone();
        let (event_tx, event_rx) = mpsc::unbounded();

        run_hosted_ordinary_turn(
            &config,
            &thread,
            hosted_turn_request(
                &SubmittedTurn::user("typed app dispatch".to_string()),
                false,
            ),
            &event_tx,
            &control,
        )
        .expect("typed ordinary turn");

        let snapshot = TuiSurfaceActions::new(thread.typed_surface())
            .read_snapshot()
            .expect("typed snapshot");
        let (user, user_is_pinned) = snapshot
            .items
            .iter()
            .find_map(|item| match item {
                orca_runtime::surface::SurfaceItem::UserMessage {
                    turn_id,
                    input: orca_runtime::surface::SurfaceUserInputState::Resolved { .. },
                    pinned,
                    ..
                } => Some((turn_id.clone(), *pinned)),
                _ => None,
            })
            .expect("resolved typed user item");
        let assistant = snapshot
            .items
            .iter()
            .find_map(|item| match item {
                orca_runtime::surface::SurfaceItem::AssistantMessage { turn_id, text, .. }
                    if text.as_str() == "Mock runtime completed the headless harness contract." =>
                {
                    Some(turn_id.clone())
                }
                _ => None,
            })
            .expect("typed assistant item");
        let reasoning = snapshot
            .items
            .iter()
            .find_map(|item| match item {
                orca_runtime::surface::SurfaceItem::AssistantReasoning {
                    turn_id, content, ..
                } if content.as_str()
                    == "Mock runtime is preserving the DeepSeek reasoning channel." =>
                {
                    Some(turn_id.clone())
                }
                _ => None,
            })
            .expect("typed reasoning item");

        assert_eq!(assistant, user);
        assert_eq!(reasoning, user);
        assert!(
            !user_is_pinned,
            "ordinary TUI input must stay unpinned so it remains backtrackable"
        );
        assert!(snapshot.foreground_operation.is_none());
        assert!(snapshot.operation_history.iter().any(|operation| {
            matches!(
                operation.terminal.as_ref().map(|record| &record.terminal),
                Some(orca_runtime::surface::OperationTerminal::Succeeded { .. })
            )
        }));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    });
}

#[test]
fn ordinary_tui_turn_runs_with_only_typed_surface_task_control() {
    with_orca_home(|_| {
        let config = test_config(HistoryMode::Record);
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .handle()
            .start_thread(config.clone(), "typed ordinary turn isolated control")
            .expect("runtime thread");
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        run_hosted_ordinary_turn(
            &config,
            &thread,
            hosted_turn_request(
                &SubmittedTurn::user("typed isolated control".to_string()),
                false,
            ),
            &event_tx,
            &control,
        )
        .expect("typed ordinary turn without legacy operation owner");

        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        assert!(control.current_id().is_none());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    });
}

#[test]
fn ordinary_tui_typed_submit_can_manual_compact_the_same_durable_conversation() {
    with_orca_home(|_| {
        let config = test_config(HistoryMode::Record);
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .handle()
            .start_thread(config.clone(), "typed ordinary turn compaction")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::new();
        let control = controller.clone();
        let (event_tx, event_rx) = mpsc::unbounded();

        run_hosted_ordinary_turn(
            &config,
            &thread,
            hosted_turn_request(
                &SubmittedTurn::user("typed app dispatch before compaction".to_string()),
                false,
            ),
            &event_tx,
            &control,
        )
        .expect("typed ordinary turn");
        let outcome = match TuiSurfaceActions::new(thread.typed_surface())
            .manual_compact(&controller.clone(), &event_tx)
        {
            Ok(outcome) => outcome,
            Err(error) => panic!(
                "typed manual compaction after ordinary turn: {error}; events={:?}",
                event_rx.try_iter().collect::<Vec<_>>()
            ),
        };

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::ManualCompaction
        ));
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TuiEvent::CompactionStarted))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TuiEvent::Compacted { .. }))
        );
        assert!(
            TuiSurfaceActions::new(thread.typed_surface())
                .read_snapshot()
                .expect("typed snapshot after compaction")
                .foreground_operation
                .is_none()
        );

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    });
}

#[test]
fn hosted_tui_foreground_turn_uses_canonical_verifier_terminal() {
    with_orca_home(|_| {
        let mut config = test_config(HistoryMode::Record);
        config.verifier = Some("false".to_string());
        let mut harness = HostedTuiHarness::start(config, None);

        harness.send(UserAction::Submit("verify canonical TUI turn".to_string()));
        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "verification_failed"
        ));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_runtime_queue_continues_after_busy_submission() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));

        harness.send(UserAction::QueuePrompt {
            prompt: "mock_history_echo".to_string(),
            bindings: orca_runtime::mentions::MentionBindings::new("mock_history_echo"),
            images: Vec::new(),
        });
        let pending = harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::PromptQueueUpdated(snapshot)
                    if snapshot.running_item().is_none()
                        && snapshot.items.iter().any(|item| item.input.text == "mock_history_echo")
            )
        });
        assert!(matches!(
            pending,
            TuiEvent::PromptQueueUpdated(snapshot)
                if snapshot.running_item().is_none()
        ));

        harness.send(UserAction::Submit("mock_history_echo".to_string()));
        harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::PromptQueueUpdated(snapshot)
                    if snapshot.items.len() >= 2 && snapshot.running_item().is_some()
            )
        });

        let second_turn = harness
            .recv_until(|event| matches!(event, TuiEvent::TurnStarted { turn, .. } if *turn >= 2));
        assert!(matches!(second_turn, TuiEvent::TurnStarted { turn, .. } if turn >= 2));
        let echo = harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("mock_history_echo"))
            });
        assert!(matches!(echo, TuiEvent::MessageDelta(text) if text.contains("mock_history_echo")));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_queued_user_input_round_trips_through_the_tui() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 500".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
        harness.send(UserAction::QueuePrompt {
            prompt: "ask continue?".to_string(),
            bindings: orca_runtime::mentions::MentionBindings::new("ask continue?"),
            images: Vec::new(),
        });
        let key = match harness
            .recv_until(|event| matches!(event, TuiEvent::UserInputRequested { .. }))
        {
            TuiEvent::UserInputRequested { key, .. } => key,
            _ => unreachable!(),
        };
        harness.send(UserAction::RespondToInteraction {
            key,
            response: TuiInteractionResponse::UserInput("yes".to_string()),
        });
        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(terminal, TuiEvent::SessionCompleted { status } if status == "success"));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_new_session_preserves_old_history_and_starts_with_empty_context() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("old conversation prompt".to_string()));
        let old_session_id =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => thread
                    .session_id()
                    .expect("old recorded session id")
                    .to_string(),
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        harness.send(UserAction::NewSession);
        let mut reset_session_id = None;
        let new_session_id = loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("new-session projection event")
            {
                TuiEvent::SessionProjectionReset(projection) => {
                    reset_session_id = projection.session_id;
                }
                TuiEvent::NewSessionStarted => {
                    break reset_session_id
                        .clone()
                        .expect("recorded new-session reset identity");
                }
                _ => {}
            }
        };

        assert_eq!(
            reset_session_id.as_deref(),
            Some(new_session_id.as_str()),
            "the authoritative reset must precede the new-session control signal"
        );
        assert_ne!(new_session_id, old_session_id);
        assert!(matches!(
            harness.config.lock().unwrap().history_mode,
            HistoryMode::Record
        ));
        let old_transcript = history::load_session(&old_session_id)
            .expect("old session remains resumable after /new");
        assert!(old_transcript.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == "old conversation prompt")
        }));
        assert_eq!(
            history::load_session("latest")
                .expect("new conversation is the latest resumable session")
                .meta
                .session_id,
            new_session_id
        );

        harness.send(UserAction::Submit("mock_history_echo".to_string()));
        let echo = harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock history users:"))
            });
        let TuiEvent::MessageDelta(echo) = echo else {
            unreachable!()
        };
        assert_eq!(echo, "Mock history users: mock_history_echo");
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        harness.shutdown();
    });
}

#[test]
fn hosted_side_switches_project_recorded_parent_and_ephemeral_side_identity() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("parent identity prompt".to_string()));
        let parent_id =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => thread
                    .session_id()
                    .expect("recorded parent session id")
                    .to_string(),
                _ => unreachable!(),
            };
        let parent_projection = harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.session_id.as_deref() == Some(parent_id.as_str())
            )
        });
        let TuiEvent::SurfaceProjectionSynced(parent_projection) = parent_projection else {
            unreachable!()
        };
        let parent_title = parent_projection.title;
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::StartSideConversation { prompt: None });
        let side_reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        let TuiEvent::SessionProjectionReset(side_projection) = side_reset else {
            unreachable!()
        };
        assert_eq!(side_projection.session_id, None);
        assert_eq!(side_projection.title, "Side conversation");

        harness.send(UserAction::ToggleSideConversation);
        let parent_reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        assert!(matches!(
            parent_reset,
            TuiEvent::SessionProjectionReset(projection)
                if projection.session_id.as_deref() == Some(parent_id.as_str())
                    && projection.title == parent_title
        ));

        harness.send(UserAction::ToggleSideConversation);
        let side_reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        assert!(matches!(
            side_reset,
            TuiEvent::SessionProjectionReset(projection)
                if projection.session_id.is_none()
                    && projection.title == "Side conversation"
        ));

        harness.send(UserAction::CloseSideConversation);
        let parent_reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        assert!(matches!(
            parent_reset,
            TuiEvent::SessionProjectionReset(projection)
                if projection.session_id.as_deref() == Some(parent_id.as_str())
                    && projection.title == parent_title
        ));
        harness.shutdown();
    });
}

#[test]
fn hosted_side_reentry_rebinds_background_presentation_to_active_attachment() {
    with_orca_home(|_| {
        let mut harness = AttachedHostedTuiHarness::start(test_config(HistoryMode::Record));
        harness.send(UserAction::Submit("parent identity prompt".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::StartSideConversation {
            prompt: Some("mock_stream_delay_ms 1500".to_string()),
        });
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let task = matching_task_update(
            harness.recv_until(|event| {
                matching_task_update(event.clone(), |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.status == orca_core::task_types::TaskStatus::Running
                        && task.is_backgrounded
                })
                .is_some()
            }),
            |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            },
        )
        .expect("captured backgrounded side task");

        harness.send(UserAction::ToggleSideConversation);
        harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SessionProjectionReset(projection) if projection.session_id.is_some()
            )
        });
        harness.send(UserAction::ToggleSideConversation);
        harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SessionProjectionReset(projection)
                    if projection.session_id.is_none() && projection.title == "Side conversation"
            )
        });

        let terminal = matching_task_update(
            harness.recv_until(|event| {
                matching_task_update(event.clone(), |candidate| {
                    candidate.id == task.id
                        && candidate.is_backgrounded
                        && candidate.status != orca_core::task_types::TaskStatus::Running
                })
                .is_some()
            }),
            |candidate| {
                candidate.id == task.id
                    && candidate.is_backgrounded
                    && candidate.status != orca_core::task_types::TaskStatus::Running
            },
        )
        .expect("terminal side task update after reentry");
        assert!(terminal.is_backgrounded);
    });
}

#[test]
fn hosted_side_background_task_foreground_uses_surface_projection() {
    with_orca_home(|_| {
        let mut harness = AttachedHostedTuiHarness::start(test_config(HistoryMode::Record));
        harness.send(UserAction::Submit("parent identity prompt".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::StartSideConversation {
            prompt: Some("mock_stream_delay_ms 3000".to_string()),
        });
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let task = matching_task_update(
            harness.recv_until(|event| {
                matching_task_update(event.clone(), |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.status == orca_core::task_types::TaskStatus::Running
                        && task.is_backgrounded
                })
                .is_some()
            }),
            |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            },
        )
        .expect("captured backgrounded side task");

        harness.send(UserAction::ForegroundTask {
            task_id: task.id.clone(),
        });
        let mut saw_output_handoff = false;
        let mut saw_foreground_projection = false;
        let mut saw_completed_delta = false;
        let mut saw_terminal_projection = false;
        let mut saw_session_completed = false;
        let returned = harness.recv_until(|event| match event {
            TuiEvent::BackgroundTaskOutputAttached { task_id } if task_id == &task.id => {
                saw_output_handoff = true;
                false
            }
            TuiEvent::SurfaceProjectionSynced(projection)
                if projection.workflow_tasks.iter().any(|candidate| {
                    candidate.id == task.id
                        && candidate.status == orca_core::task_types::TaskStatus::Running
                        && !candidate.is_backgrounded
                }) =>
            {
                saw_foreground_projection = true;
                false
            }
            TuiEvent::MessageDelta(text) if text.contains("Mock slow stream completed.") => {
                saw_completed_delta = true;
                false
            }
            TuiEvent::SurfaceProjectionSynced(projection)
                if projection.workflow_tasks.iter().any(|candidate| {
                    candidate.id == task.id
                        && !candidate.is_backgrounded
                        && matches!(
                            candidate.status,
                            orca_core::task_types::TaskStatus::Completed
                                | orca_core::task_types::TaskStatus::Failed
                                | orca_core::task_types::TaskStatus::Cancelled
                                | orca_core::task_types::TaskStatus::Stopped
                        )
                }) =>
            {
                saw_terminal_projection = true;
                false
            }
            TuiEvent::SessionCompleted { .. } => {
                saw_session_completed = true;
                false
            }
            TuiEvent::Notice(message)
                if message == &format!("Task {} returned to foreground.", task.id) =>
            {
                true
            }
            TuiEvent::Error(message) => panic!("foreground task failed: {message}"),
            _ => false,
        });
        assert!(matches!(returned, TuiEvent::Notice(_)));
        assert!(
            saw_output_handoff,
            "ephemeral foreground must attach output before reporting success"
        );
        assert!(
            saw_foreground_projection,
            "ephemeral foreground must publish its accepted surface task projection"
        );

        if !saw_session_completed {
            harness.recv_until(|event| match event {
                TuiEvent::SessionCompleted { .. } => {
                    saw_session_completed = true;
                    true
                }
                TuiEvent::Error(message) => {
                    panic!("foreground task continuation failed: {message}")
                }
                _ => false,
            });
        }
        assert!(saw_session_completed);
        assert!(
            saw_completed_delta,
            "foreground task must continue through its terminal output"
        );
        assert!(
            saw_terminal_projection,
            "foreground task must publish its terminal surface projection"
        );
    });
}

#[test]
fn session_preflight_failure_preserves_previous_runtime_and_projection() {
    with_orca_home(|_| {
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let config = test_config(HistoryMode::Record);
        let previous = host
            .handle()
            .start_thread(config.clone(), "Previous conversation")
            .expect("previous runtime thread");
        let previous_projection = TuiSurfaceActions::new(previous.typed_surface())
            .read_snapshot()
            .map(|snapshot| SurfaceProjectionState::from_surface_snapshot(&snapshot))
            .expect("previous projection");
        let previous_id = previous_projection
            .session_id
            .clone()
            .expect("previous recorded session id");
        let (mut state, _actions) = test_state();
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            previous_projection,
        )));

        let uninstalled = host
            .handle()
            .start_thread(config, "Uninstalled conversation")
            .expect("uninstalled runtime thread");
        uninstalled.shutdown().expect("close uninstalled thread");
        let error = match preflight_started_session(uninstalled, "test failed switch") {
            Ok(_) => panic!("closed uninstalled thread must fail projection preflight"),
            Err(error) => error,
        };

        assert!(error.contains("failed to project conversation before test failed switch"));
        assert_eq!(previous.session_id(), Some(previous_id.as_str()));
        assert!(
            TuiSurfaceActions::new(previous.typed_surface())
                .read_snapshot()
                .is_ok()
        );
        assert_eq!(state.current_session_id(), Some(previous_id.as_str()));
        assert_eq!(state.current_session_title(), Some("Previous conversation"));

        previous.shutdown().expect("previous thread shutdown");
        host.shutdown().expect("runtime host shutdown");
    });
}

#[test]
fn hosted_tui_fork_preserves_source_and_projects_copied_history() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("investigate auth".to_string()));
        let source_id =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => {
                    thread.session_id().expect("source session id").to_string()
                }
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::ForkCurrentSession {
            title: Some("Auth experiment".to_string()),
        });
        let fork_projection = harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.session_id.as_deref() != Some(source_id.as_str())
                        && projection.session_presentation
                            == Some(SessionProjectionPresentation::Forked)
            )
        });
        let TuiEvent::SurfaceProjectionSynced(fork_projection) = fork_projection else {
            unreachable!()
        };
        assert_eq!(fork_projection.title, "Auth experiment");
        let fork_id = fork_projection
            .session_id
            .expect("forked recorded session id");

        assert_ne!(fork_id, source_id);
        let source = history::load_session(&source_id).expect("source remains loadable");
        let fork = history::load_session(&fork_id).expect("fork remains loadable");
        assert_eq!(fork.meta.parent_id.as_deref(), Some(source_id.as_str()));
        assert_eq!(fork.meta.title, "Auth experiment");
        assert!(fork.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == "investigate auth")
        }));
        assert!(source.meta.parent_id.is_none());
        assert!(matches!(
            harness.config.lock().unwrap().history_mode,
            HistoryMode::Fork(ref selector) if selector == &source_id
        ));
        harness.shutdown();
    });
}

#[test]
fn picker_fork_replaces_source_transcript() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("source-only prompt".to_string()));
        let source_id =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => {
                    thread.session_id().expect("source session id").to_string()
                }
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        let source_title = history::load_session(&source_id)
            .expect("source transcript")
            .meta
            .title;

        harness.send(UserAction::NewSession);
        let mut current_id = None;
        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("new-session event")
            {
                TuiEvent::SessionProjectionReset(projection) => {
                    current_id = projection.session_id;
                }
                TuiEvent::NewSessionStarted => break,
                _ => {}
            }
        }
        let current_id = current_id.expect("current recorded session id");
        harness.send(UserAction::Submit("current-only prompt".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::ForkSavedSession {
            session_id: source_id.clone(),
        });
        let reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        let reset_session_id = match reset {
            TuiEvent::SessionProjectionReset(projection) => {
                assert_eq!(projection.title, format!("Fork of {source_title}"));
                projection.session_id.expect("saved fork reset identity")
            }
            _ => unreachable!(),
        };
        let history = harness.recv_until(|event| matches!(event, TuiEvent::HistoryLoaded { .. }));
        let TuiEvent::HistoryLoaded { messages, .. } = history else {
            unreachable!();
        };
        assert!(
            messages.iter().any(|message| {
                matches!(message, ChatMessage::User(prompt) if prompt == "source-only prompt")
            }),
            "fork history messages: {messages:?}"
        );
        assert!(!messages.iter().any(|message| {
            matches!(message, ChatMessage::User(prompt) if prompt == "current-only prompt")
        }));

        let fork_projection = harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.session_presentation
                        == Some(SessionProjectionPresentation::Forked)
            )
        });
        let TuiEvent::SurfaceProjectionSynced(fork_projection) = fork_projection else {
            unreachable!()
        };
        let fork_id = fork_projection
            .session_id
            .expect("saved fork projection identity");
        assert_ne!(fork_id, source_id);
        assert_ne!(fork_id, current_id);
        assert_eq!(fork_id, reset_session_id);
        harness.shutdown();
    });
}

#[test]
fn picker_resume_requires_authoritative_session_reset() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("resume-source prompt".to_string()));
        let source_id =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => {
                    thread.session_id().expect("source session id").to_string()
                }
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        let source_title = history::load_session(&source_id)
            .expect("source transcript")
            .meta
            .title;

        harness.send(UserAction::NewSession);
        harness.recv_until(|event| matches!(event, TuiEvent::NewSessionStarted));
        harness.send(UserAction::Submit("current-only prompt".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        harness.send(UserAction::ResumeSavedSession {
            session_id: source_id.clone(),
        });
        let reset =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionProjectionReset(_)));
        assert!(matches!(
            reset,
            TuiEvent::SessionProjectionReset(projection)
                if projection.session_id.as_deref() == Some(source_id.as_str())
                    && projection.title == source_title
        ));
        let history = harness.recv_until(|event| matches!(event, TuiEvent::HistoryLoaded { .. }));
        let TuiEvent::HistoryLoaded { messages, .. } = history else {
            unreachable!();
        };
        assert!(messages.iter().any(|message| {
            matches!(message, ChatMessage::User(prompt) if prompt == "resume-source prompt")
        }));
        assert!(!messages.iter().any(|message| {
            matches!(message, ChatMessage::User(prompt) if prompt == "current-only prompt")
        }));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_rename_updates_durable_title_and_projection_event() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("release notes".to_string()));
        let (session_id, surface) =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => (
                    thread
                        .session_id()
                        .expect("recorded session id")
                        .to_string(),
                    thread,
                ),
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        let stale_revision = TuiSurfaceActions::new(surface.clone())
            .read_snapshot()
            .expect("initial typed surface")
            .thread
            .metadata_revision;

        harness.send(UserAction::RenameCurrentSession {
            title: "Release triage".to_string(),
        });
        let renamed = loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("rename projection event")
            {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.session_id.as_deref() == Some(session_id.as_str())
                        && projection.title == "Release triage" =>
                {
                    break projection;
                }
                _ => {}
            }
        };

        assert_eq!(renamed.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(renamed.title, "Release triage");
        assert_eq!(
            renamed.session_presentation,
            Some(SessionProjectionPresentation::Renamed)
        );
        assert_eq!(
            history::load_session(&session_id)
                .expect("renamed session")
                .meta
                .title,
            "Release triage"
        );
        assert_eq!(
            TuiSurfaceActions::new(surface.clone())
                .read_snapshot()
                .expect("renamed typed surface")
                .thread
                .title
                .as_str(),
            "Release triage"
        );
        assert!(
            crate::surface_client::update_session_metadata(
                &surface,
                stale_revision,
                orca_runtime::surface::SessionMetadataPatch::SetTitle {
                    title: orca_runtime::surface::DisplayText::new("stale title"),
                },
            )
            .is_err_and(|error| error.is_stale())
        );
        assert_eq!(
            TuiSurfaceActions::new(surface)
                .read_snapshot()
                .expect("typed surface after stale patch")
                .thread
                .title
                .as_str(),
            "Release triage"
        );
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_rename_restores_runtime_projection_when_durable_write_fails() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("original title".to_string()));
        let (session_id, surface) =
            match harness.recv_until(|event| matches!(event, TuiEvent::MentionRuntimeReady(_))) {
                TuiEvent::MentionRuntimeReady(thread) => (
                    thread
                        .session_id()
                        .expect("recorded session id")
                        .to_string(),
                    thread,
                ),
                _ => unreachable!(),
            };
        harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        let original_title = TuiSurfaceActions::new(surface.clone())
            .read_snapshot()
            .expect("original typed surface")
            .thread
            .title
            .as_str()
            .to_string();

        crate::surface_actions::inject_rename_saved_session_failure_once(&session_id);
        harness.send(UserAction::RenameCurrentSession {
            title: "must not stick".to_string(),
        });
        let rejected = harness
                .recv_until(|event| matches!(event, TuiEvent::OperationRejected(message) if message.contains("failed to persist conversation rename")));

        assert!(matches!(rejected, TuiEvent::OperationRejected(_)));
        assert_eq!(
            history::load_session(&session_id)
                .expect("unchanged durable session")
                .meta
                .title,
            original_title
        );
        assert_eq!(
            TuiSurfaceActions::new(surface)
                .read_snapshot()
                .expect("compensated typed surface")
                .thread
                .title
                .as_str(),
            original_title
        );
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_new_session_rejects_active_background_work() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 3000".to_string()));
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let task = loop {
            let event = harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("backgrounded task update");
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            }) {
                break task;
            }
        };

        harness.send(UserAction::NewSession);
        let rejected = harness.recv_until(|event| {
                matches!(event, TuiEvent::OperationRejected(message) if message.contains("active work"))
            });
        assert!(matches!(rejected, TuiEvent::OperationRejected(_)));

        harness.send(UserAction::StopTask {
            task_id: task.id.clone(),
        });
        harness.recv_until(|event| {
            matching_task_update(event.clone(), |candidate| {
                candidate.id == task.id
                    && candidate.status == orca_core::task_types::TaskStatus::Cancelled
            })
            .is_some()
        });
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_background_handoff_without_capacity_completes_successfully() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start_with_background_capacity(
            test_config(HistoryMode::Record),
            None,
            0,
        );
        harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));

        // With no background capacity the handoff auto-queues the turn
        // instead of failing the session; the queued turn then runs to
        // completion, so the session terminal reports success.
        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        assert_eq!(harness.runtime.controller().current_id(), None);
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_backgrounded_canonical_provider_can_be_stopped_once() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let task = loop {
            let event = harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("backgrounded task update");
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            }) {
                break task;
            }
        };

        harness.send(UserAction::StopTask {
            task_id: task.id.clone(),
        });
        let stopped = loop {
            let event = harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("stopped task update");
            if let Some(task) = matching_task_update(event, |candidate| {
                candidate.id == task.id
                    && candidate.status == orca_core::task_types::TaskStatus::Cancelled
            }) {
                break task;
            }
        };
        assert!(stopped.is_backgrounded);

        harness.send(UserAction::StopTask {
            task_id: task.id.clone(),
        });
        let duplicate_stop = harness.recv_until(
                |event| matches!(event, TuiEvent::Error(message) if message.contains("already cancelled")),
            );
        assert!(matches!(duplicate_stop, TuiEvent::Error(_)));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_backgrounded_canonical_provider_can_be_foregrounded_once() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 3000".to_string()));
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::BackgroundCurrentTurn);
        let task = loop {
            let event = harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("backgrounded task update");
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            }) {
                break task;
            }
        };

        harness.send(UserAction::ForegroundTask {
            task_id: task.id.clone(),
        });
        harness.recv_until(|event| {
            matching_task_update(event.clone(), |candidate| {
                candidate.id == task.id
                    && candidate.status == orca_core::task_types::TaskStatus::Running
                    && !candidate.is_backgrounded
            })
            .is_some()
        });

        let mut saw_completed_delta = false;
        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("foregrounded provider completion")
            {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow stream completed.") => {
                    saw_completed_delta = true;
                }
                TuiEvent::SessionCompleted { status } => {
                    assert_eq!(status, "success");
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_completed_delta);

        harness.send(UserAction::ForegroundTask {
            task_id: task.id.clone(),
        });
        let duplicate = harness.recv_until(|event| matches!(event, TuiEvent::Error(_)));
        assert!(
            matches!(
                duplicate,
                TuiEvent::Error(ref message) if message.contains("already delivered")
            ),
            "unexpected duplicate foreground result: {duplicate:?}"
        );
        harness.shutdown();
    });
}

#[test]
fn hosted_canonical_approval_uses_operation_fence_and_resumes_turn() {
    with_orca_home(|home| {
        std::fs::write(home.join("approval.txt"), "old").expect("approval fixture");
        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(home.to_path_buf());
        let mut harness = HostedTuiHarness::start(config, None);
        harness.send(UserAction::Submit(
            "edit approval.txt :: old => new".to_string(),
        ));

        let key = match harness.recv_until(|event| matches!(event, TuiEvent::ApprovalNeeded { .. }))
        {
            TuiEvent::ApprovalNeeded { key, .. } => key,
            _ => unreachable!(),
        };
        assert!(harness.runtime.controller().has_surface_active());
        harness.send(UserAction::RespondToInteraction {
            key,
            response: TuiInteractionResponse::Approval(true),
        });

        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        assert_eq!(
            std::fs::read_to_string(home.join("approval.txt")).unwrap(),
            "new"
        );
        harness.shutdown();
    });
}

#[test]
fn hosted_canonical_permission_uses_operation_fence_and_resumes_turn() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit(
            "request_network_permissions_then_done example.com".to_string(),
        ));

        let key = match harness
            .recv_until(|event| matches!(event, TuiEvent::PermissionApprovalNeeded { .. }))
        {
            TuiEvent::PermissionApprovalNeeded { key, .. } => key,
            _ => unreachable!(),
        };
        assert!(harness.runtime.controller().has_surface_active());
        harness.send(UserAction::RespondToInteraction {
            key,
            response: TuiInteractionResponse::Permission(true),
        });

        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        harness.shutdown();
    });
}

#[test]
fn hosted_canonical_user_input_uses_operation_fence_and_resumes_turn() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("ask continue?".to_string()));

        let key = match harness
            .recv_until(|event| matches!(event, TuiEvent::UserInputRequested { .. }))
        {
            TuiEvent::UserInputRequested { key, .. } => key,
            _ => unreachable!(),
        };
        assert!(harness.runtime.controller().has_surface_active());
        harness.send(UserAction::RespondToInteraction {
            key,
            response: TuiInteractionResponse::UserInput("yes".to_string()),
        });

        let terminal =
            harness.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        harness.shutdown();
    });
}

#[test]
fn hosted_tui_interrupt_targets_activation_race_and_waits_for_terminal() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let pending = test_pending_workflow_notifications();
        let (event_tx, event_rx) = mpsc::unbounded();
        let event_tx = spawn_unwrapped_tui_test_event_sender(event_tx);
        let (action_tx, action_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::new();
        let mut runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx.clone(),
            8,
            controller,
            move |controller, commands, host| {
                hosted_tui_controller_loop(
                    config, preloaded, event_tx, commands, controller, pending, host,
                );
            },
        )
        .expect("hosted TUI runtime");

        action_tx
            .send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()))
            .unwrap();
        action_tx.send(UserAction::Interrupt).unwrap();
        loop {
            if let TuiEvent::SessionCompleted { status } =
                event_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                assert_eq!(status, "cancelled");
                assert_eq!(runtime.controller().current_id(), None);
                break;
            }
        }
        runtime.shutdown().expect("hosted runtime shutdown");
    });
}

#[test]
fn hosted_submission_start_failure_rejects_prompt_and_preserves_preloaded() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(Some(transcript("preserved-session"))));
        let (event_tx, event_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::new();
        let pending = test_pending_workflow_notifications();
        let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
        let host_handle = host.handle();
        host.shutdown().unwrap();
        let mut thread = None;

        handle_hosted_submitted_turn(
            SubmittedTurn::user("retry me".to_string()),
            &config,
            &preloaded,
            &mut thread,
            &event_tx,
            &controller.clone(),
            &pending,
            &host_handle,
        );

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::SubmissionRejected {
                prompt, message, ..
            })
                if prompt == "retry me"
                    && message.contains("failed to initialize conversation history")
        ));
        assert!(thread.is_none());
        assert_eq!(
            preloaded
                .lock()
                .unwrap()
                .as_ref()
                .map(|transcript| transcript.meta.session_id.as_str()),
            Some("preserved-session")
        );
    });
}

#[test]
fn remember_slash_command_dispatches_scope_without_writing_memory() {
    with_orca_home(|home| {
        let mut config = test_config(HistoryMode::Record);
        config.cwd = Some(home.to_path_buf());
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (mut state, _) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();

        handle_slash_command(
            "/remember project: prefer runtime ownership",
            &mut config,
            &shared_config,
            &mut state,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::Remember {
                scope: crate::protocol::TuiMemoryScope::Project,
                note,
            }) if note == "prefer runtime ownership"
        ));
        assert!(
            orca_runtime::memory::load_for_cwd(home).is_empty(),
            "the renderer-side slash action must not persist memory"
        );
    });
}

#[test]
fn resume_slash_command_never_implicitly_resumes_a_recoverable_operation() {
    with_orca_home(|_| {
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (mut state, _) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            recovery_projection_for_test(
                orca_runtime::surface::SurfaceOperationId::try_from_bytes([
                    0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1,
                ])
                .unwrap(),
            ),
        )));

        handle_slash_command(
            "/resume",
            &mut config,
            &shared_config,
            &mut state,
            &action_tx,
        );

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::Idle);
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::System(message)) if message == "No saved conversations."
        ));
    });
}

#[test]
fn cancel_operation_slash_command_dispatches_explicit_runtime_action() {
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));

    let (mut cancel_state, _) = test_state();
    let (cancel_tx, cancel_rx) = mpsc::unbounded();
    let cancel_operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2,
    ])
    .unwrap();
    cancel_state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        recovery_projection_for_test(cancel_operation_id.clone()),
    )));
    handle_slash_command(
        "/cancel-operation",
        &mut config,
        &shared_config,
        &mut cancel_state,
        &cancel_tx,
    );

    assert!(matches!(
        cancel_rx.try_recv(),
        Ok(UserAction::CancelOperation { operation_id })
            if operation_id == cancel_operation_id
    ));
    assert_eq!(cancel_state.status, AppStatus::Running);
}

#[test]
fn new_and_clear_slash_commands_dispatch_the_same_session_action() {
    for command in ["/new", "/clear"] {
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (mut state, _) = test_state();
        let (action_tx, action_rx) = mpsc::unbounded();

        handle_slash_command(command, &mut config, &shared_config, &mut state, &action_tx);

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::NewSession)));
        assert_eq!(state.status, AppStatus::Running);
    }
}

#[test]
fn new_slash_command_is_rejected_while_waiting_for_user_input() {
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (mut state, _) = test_state();
    state.status = AppStatus::WaitingUserInput;
    let (action_tx, action_rx) = mpsc::unbounded();

    handle_slash_command("/new", &mut config, &shared_config, &mut state, &action_tx);

    assert!(action_rx.try_recv().is_err());
    assert_eq!(state.status, AppStatus::WaitingUserInput);
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Error(message)) if message.contains("finish or cancel")
    ));
}

#[test]
fn queued_goal_preflight_failure_preserves_queued_identity() {
    with_orca_home(|_| {
        let cfg = test_config(HistoryMode::Disabled);
        let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
        let runtime_thread = host
            .start_thread(cfg.clone(), "queued preflight failure")
            .unwrap();
        let controller = TuiSurfaceTaskControl::new();
        let control = controller.clone();
        let (event_tx, event_rx) = mpsc::unbounded();

        run_hosted_goal_run(
            &cfg,
            &runtime_thread,
            SubmittedTurn::queued_user_with_mentions(
                42,
                "restore queued prompt".to_string(),
                orca_runtime::mentions::MentionBindings::new("restore queued prompt"),
                Vec::new(),
            ),
            orca_core::goal_runtime::GoalTurnOrigin::User,
            &event_tx,
            &control,
        );

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::SubmissionRejected {
                queued_id: Some(42),
                prompt,
                message,
                ..
            }) if prompt == "restore queued prompt"
                && message.contains("persistent goals require recorded history")
        ));
        host.shutdown().unwrap();
    });
}

#[test]
fn hosted_tui_shutdown_cancels_and_joins_active_operation() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let pending = test_pending_workflow_notifications();
        let (event_tx, event_rx) = mpsc::unbounded();
        let event_tx = spawn_unwrapped_tui_test_event_sender(event_tx);
        let (action_tx, action_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::new();
        let mut runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx.clone(),
            8,
            controller,
            move |controller, commands, host| {
                hosted_tui_controller_loop(
                    config, preloaded, event_tx, commands, controller, pending, host,
                );
            },
        )
        .unwrap();

        action_tx
            .send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()))
            .unwrap();
        loop {
            if matches!(
                event_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                TuiEvent::TurnStarted { .. }
            ) {
                break;
            }
        }

        runtime.shutdown().expect("hosted runtime shutdown");
    });
}

#[test]
fn controller_exit_joins_visible_side_before_releasing_parent() {
    with_orca_home(|_| {
        let config = test_config(HistoryMode::Disabled);
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let parent = host
            .handle()
            .start_thread(config.clone(), "main conversation")
            .expect("parent thread");
        let side = host
            .handle()
            .start_side_thread(&parent, config.clone(), "Side conversation")
            .expect("side thread");
        let parent_probe = parent.clone();
        let side_probe = side.clone();
        let (parent_event_tx, _parent_event_rx) = mpsc::unbounded();
        let (side_event_tx, _side_event_rx) = mpsc::unbounded();

        shutdown_attached_side_on_controller_exit(HostedSideParent {
            thread: parent,
            event_tx: parent_event_tx,
            attachment: SessionAttachmentId::new(1),
            side_thread: side,
            side_event_tx,
            side_attachment: SessionAttachmentId::new(2),
            side_config: Arc::new(Mutex::new(config)),
            parent_title: "main conversation".to_string(),
        });

        assert!(
            !side_probe.is_available(),
            "Side actor must be joined first"
        );
        assert!(
            !parent_probe.is_available(),
            "parent actor must also be joined"
        );
        host.shutdown().expect("runtime host shutdown");
    });
}

#[test]
fn running_background_shortcut_dispatches_action_and_returns_to_idle_without_cancelling() {
    let (mut state, action_rx) = test_state();
    state.status = AppStatus::Running;
    let action_tx = state.event_tx.clone();

    crate::running_actions::handle_running_shortcut(
        crate::shortcuts::RunningShortcut::BackgroundCurrentTurn,
        &mut state,
        &action_tx,
    );

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::BackgroundCurrentTurn)
    ));
    assert_eq!(state.status, AppStatus::Idle);
}

#[test]
fn empty_recorded_session_goal_show_dispatches_agent_action() {
    let (mut state, rx) = test_state();
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));

    handle_slash_command("/goal", &mut config, &shared_config, &mut state, &action_tx);

    assert!(rx.try_recv().is_err());
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::GoalShow)));
    assert_eq!(state.status, AppStatus::Running);
}

#[test]
fn empty_recorded_hosted_tui_goal_show_reports_no_goal() {
    let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
    let preloaded = Arc::new(Mutex::new(None));
    let (event_tx, event_rx) = mpsc::unbounded();
    let (action_tx, action_rx) = mpsc::unbounded();
    let cancel = CancelToken::new();

    let handle = std::thread::spawn({
        let config = Arc::clone(&config);
        let preloaded = Arc::clone(&preloaded);
        let cancel = cancel.clone();
        move || {
            run_hosted_tui_controller_for_test(
                config,
                preloaded,
                event_tx,
                action_rx,
                cancel,
                test_pending_workflow_notifications(),
            )
        }
    });

    action_tx.send(UserAction::GoalShow).unwrap();
    let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    action_tx.send(UserAction::Cancel).unwrap();
    handle.join().unwrap();

    assert!(matches!(event, TuiEvent::GoalStatus(None)));
}

#[test]
fn empty_recorded_hosted_tui_goal_controls_report_session_not_started() {
    let cases = [
        UserAction::GoalEdit("better goal".to_string().into()),
        UserAction::GoalClear,
        UserAction::GoalPause,
    ];

    for action in cases {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(action).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        match event {
            TuiEvent::Error(message) => {
                assert_eq!(
                    message,
                    "The session must start before you can change a goal."
                );
            }
            other => panic!("expected goal control error, got {other:?}"),
        }
    }
}

#[test]
fn empty_recorded_hosted_tui_goal_resume_without_active_goal_reports_none() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalResume).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        cancel.cancel();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(matches!(event, TuiEvent::GoalStatus(None)));
    });
}

#[test]
fn resumed_uuid_session_emits_typed_history_before_accepting_initial_turn() {
    with_orca_home(|_| {
        let mut source = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        let secret = "sk-test-typed-history-secret-1234567890";
        source.send(UserAction::Submit(format!(
            "restored prompt api_key={secret}"
        )));
        let source_terminal =
            source.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(
            source_terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        source.send(UserAction::Submit("mock_history_echo".to_string()));
        let echo_terminal =
            source.recv_until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(matches!(
            echo_terminal,
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
        source.shutdown();
        let source_transcript = history::load_session("latest").unwrap();
        let session_id = source_transcript.meta.session_id;
        let source_title = source_transcript.meta.title;

        let mut harness =
            HostedTuiHarness::start(test_config(HistoryMode::Resume(session_id.clone())), None);
        let event = harness
            .event_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("typed history event");
        let TuiEvent::HistoryLoaded { messages, .. } = event else {
            panic!("expected typed history");
        };
        assert!(messages.iter().any(|message| matches!(
            message,
            ChatMessage::User(prompt)
                if prompt == "restored prompt api_key=<redacted>"
        )));
        assert!(
            !messages
                .iter()
                .any(|message| format!("{message:?}").contains(secret)),
            "typed restart history must not display the replay secret"
        );
        let reasoning_index = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ChatMessage::Reasoning(reasoning)
                        if reasoning == "Mock runtime is preserving the DeepSeek reasoning channel."
                )
            })
            .expect("typed reasoning history");
        let assistant_index = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ChatMessage::Assistant(answer)
                        if answer == "Mock runtime completed the headless harness contract."
                )
            })
            .expect("typed assistant history");
        assert!(
            reasoning_index < assistant_index,
            "restart history must preserve the live reasoning-before-assistant order"
        );
        assert!(messages.iter().any(|message| matches!(
                message,
                ChatMessage::Assistant(answer)
                    if answer == "Mock history users: restored prompt api_key=<redacted> | mock_history_echo"
            )));
        let projection = harness.recv_until(|event| {
            matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.session_id.as_deref() == Some(session_id.as_str())
            )
        });
        let TuiEvent::SurfaceProjectionSynced(projection) = projection else {
            unreachable!()
        };
        assert_eq!(projection.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            projection.title, source_title,
            "startup projection must preserve durable title"
        );

        harness.send(UserAction::Submit("mock_history_echo".to_string()));
        let mut saw_restored_history = false;
        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("resumed typed TUI event")
            {
                TuiEvent::MessageDelta(text) if text.contains("restored prompt") => {
                    saw_restored_history = true;
                }
                TuiEvent::SessionCompleted { status } => {
                    assert_eq!(status, "success");
                    break;
                }
                TuiEvent::Error(message) | TuiEvent::OperationRejected(message) => {
                    panic!("resumed typed TUI turn failed: {message}");
                }
                _ => {}
            }
        }
        assert!(
            saw_restored_history,
            "resumed typed turn must receive durable history"
        );
        harness.shutdown();
    });
}

#[test]
fn resumed_legacy_usage_projects_context_before_next_turn() {
    with_orca_home(|home| {
        let mut writer =
            history::SessionWriter::start(home, "mock", Some("auto".to_string()), "context")
                .expect("create legacy context transcript");
        writer
            .append_usage(orca_core::cost_types::UsageTotals {
                input_tokens: 929_128,
                output_tokens: 10_260,
                cache_tokens: 893_696,
                estimated_cost_usd: 0.063661744,
            })
            .expect("append penultimate usage snapshot");
        writer
            .append_usage(orca_core::cost_types::UsageTotals {
                input_tokens: 970_611,
                output_tokens: 10_627,
                cache_tokens: 935_040,
                estimated_cost_usd: 0.065860634,
            })
            .expect("append latest usage snapshot");
        drop(writer);
        let session_id = history::load_session("latest")
            .expect("load legacy context transcript")
            .meta
            .session_id;

        let mut harness =
            HostedTuiHarness::start(test_config(HistoryMode::Resume(session_id)), None);
        let event =
            harness.recv_until(|event| matches!(event, TuiEvent::SurfaceProjectionSynced(_)));
        let TuiEvent::SurfaceProjectionSynced(projection) = event else {
            unreachable!("predicate accepted only a surface projection")
        };
        assert_eq!(projection.context_used_tokens, 41_483);
        assert_eq!(projection.context_limit_tokens, 1_000_000);
        let expected_usage = projection.usage.clone();

        let (mut resumed_state, _action_rx) = test_state();
        resumed_state.update(TuiEvent::SurfaceProjectionSynced(projection));
        assert_eq!(resumed_state.usage(), &expected_usage);
        assert_eq!(resumed_state.context_used_tokens(), 41_483);
        assert_eq!(resumed_state.context_limit_tokens(), 1_000_000);

        harness.shutdown();
    });
}

#[test]
fn empty_recorded_hosted_tui_goal_resume_restores_latest_active_goal() {
    with_orca_home(|home| {
        let mut writer =
            history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal").unwrap();
        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        writer
            .append_message(&orca_core::conversation::Message::user(
                "previous goal work".to_string(),
            ))
            .unwrap();
        writer.complete("approval_required").unwrap();
        let old_session_id = history::load_session("latest").unwrap().meta.session_id;

        let goal_store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
        let created = goal_store
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: old_session_id.clone(),
                objective: "resume me".to_string(),
                token_budget: Some(80_000),
                now: 1,
            })
            .unwrap();
        goal_store
            .record_usage_once(orca_runtime::goal_store::GoalUsageEvent {
                usage_event_id: format!("test:{old_session_id}:usage"),
                goal_id: created.goal_id,
                source: "test".to_string(),
                usage: orca_core::goal_runtime::GoalUsage {
                    charged_input_tokens: 23_456,
                    elapsed_seconds: 13 * 60,
                    ..Default::default()
                },
                created_at: 2,
            })
            .unwrap();
        let original = goal_store
            .project_thread_goal(&old_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(original.token_budget, Some(80_000));

        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalResume).unwrap();
        let projection = loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_some() =>
                {
                    break projection;
                }
                TuiEvent::Error(message) => {
                    panic!("unexpected Goal resume error: {message}")
                }
                _ => {}
            }
        };
        action_tx.send(UserAction::Interrupt).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        let goal = projection.current_goal.expect("resumed Goal projection");
        assert_eq!(goal.objective, "resume me");
        assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
        // Resume continues the same thread: the goal must stay on
        // the original session id; only fork mints a new one.
        assert_eq!(goal.session_id, old_session_id);
        assert_eq!(goal.token_budget, Some(80_000));
        assert_eq!(goal.tokens_used, 23_456);
        assert_eq!(goal.time_used_seconds, 13 * 60);
        assert!(
            goal.created_at > 0,
            "typed Goal presentation uses the owning thread timestamp"
        );
        let resumed_session_id = goal.session_id;
        let store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
        let persisted = store
            .project_thread_goal(&resumed_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.status,
            orca_core::goal_types::ThreadGoalStatus::Paused,
            "interrupting the resumed Goal generation must stop automatic continuation"
        );
        assert_eq!(persisted.token_budget, Some(80_000));
        assert_eq!(persisted.objective, original.objective);
        assert_eq!(persisted.created_at, original.created_at);
        assert!(persisted.tokens_used >= original.tokens_used);
        assert!(persisted.time_used_seconds >= original.time_used_seconds);
    });
}

#[test]
fn goal_auto_continuation_pauses_after_three_no_progress_turns() {
    with_orca_home(|_home| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::GoalSet(
                "stall detection goal".to_string().into(),
            ))
            .unwrap();

        // mock provider 不产生 usage，goal 一直 active：
        // 用户 turn 后应跑满 3 个无结构化进展 turn，然后暂停并停。
        let mut stalled_notice = false;
        let mut stalled_status = false;
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline && !(stalled_notice && stalled_status) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match event_rx.recv_timeout(remaining.min(Duration::from_secs(2))) {
                Ok(TuiEvent::Notice(message)) if message.contains("no measurable progress") => {
                    seen.push(format!("notice: {message}"));
                    stalled_notice = true;
                }
                Ok(TuiEvent::SurfaceProjectionSynced(projection))
                    if projection.current_goal.as_ref().is_some_and(|goal| {
                        goal.status == orca_core::goal_types::ThreadGoalStatus::Stalled
                    }) =>
                {
                    seen.push(format!("goal: {:?}", projection.current_goal));
                    stalled_status = true;
                }
                Ok(event) => seen.push(format!("{event:?}")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("hosted TUI event channel disconnected before stall detection")
                }
            }
        }
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(stalled_notice, "missing stall notice; saw {seen:?}");
        assert!(stalled_status, "missing Stalled goal update; saw {seen:?}");
    });
}

#[test]
fn goal_resume_ignores_legacy_json_temp_directory() {
    with_orca_home(|home| {
        let mut writer =
            history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal").unwrap();
        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        writer
            .append_message(&orca_core::conversation::Message::user(
                "previous goal work".to_string(),
            ))
            .unwrap();
        writer.complete("approval_required").unwrap();
        let old_session_id = history::load_session("latest").unwrap().meta.session_id;

        orca_runtime::goal_store::GoalStore::load_default()
            .unwrap()
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: old_session_id.clone(),
                objective: "resume atomically".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();
        std::fs::create_dir(home.join("goals_1.json.tmp")).unwrap();

        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);

        harness.send(UserAction::GoalResume);
        let event = harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_some())
        });

        match event {
            TuiEvent::SurfaceProjectionSynced(projection) => {
                let goal = projection.current_goal.expect("resumed Goal projection");
                assert_eq!(goal.objective, "resume atomically");
                assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
            }
            other => panic!("expected resumed Goal projection, got {other:?}"),
        }
        assert!(matches!(
            &harness.config.lock().unwrap().history_mode,
            HistoryMode::Resume(session_id) if session_id == &old_session_id
        ));
        assert!(harness.preloaded.lock().unwrap().is_none());
        harness.shutdown();
    });
}

#[test]
fn preloaded_goal_resume_projects_elapsed_before_first_turn_started() {
    with_orca_home(|_| {
        let session_id = "019f8a00-0000-7000-8000-000000000001";
        let goal_store = orca_runtime::goal_store::GoalStore::load_default().unwrap();
        let created = goal_store
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "resume with elapsed time".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();
        goal_store
            .record_usage_once(orca_runtime::goal_store::GoalUsageEvent {
                usage_event_id: format!("test:{session_id}:elapsed"),
                goal_id: created.goal_id,
                source: "test".to_string(),
                usage: orca_core::goal_runtime::GoalUsage {
                    charged_input_tokens: 23_456,
                    elapsed_seconds: 13 * 60,
                    ..Default::default()
                },
                created_at: 2,
            })
            .unwrap();
        let persisted = goal_store.project_thread_goal(session_id).unwrap().unwrap();
        assert_eq!(persisted.time_used_seconds, 13 * 60);

        let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
            session_id.to_string(),
        ))));
        let fixture = transcript(session_id);
        history::SessionWriter::start_from_meta(fixture.meta)
            .expect("create resumable goal transcript");
        let restored = history::load_session(session_id).expect("load resumable goal transcript");
        let preloaded = Arc::new(Mutex::new(Some(restored)));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit("mock_stream_delay_ms 250".to_string()))
            .unwrap();
        let mut projected_goal = None;
        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::GoalStatus(Some(goal)) if goal.session_id == session_id => {
                    projected_goal = Some(goal);
                }
                TuiEvent::TurnStarted { .. } => break,
                TuiEvent::Error(message) => panic!("unexpected resume error: {message}"),
                _ => {}
            }
        }

        action_tx.send(UserAction::Interrupt).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        let projected_goal =
            projected_goal.expect("active GoalStatus with elapsed time must precede TurnStarted");
        assert_eq!(projected_goal.time_used_seconds, 13 * 60);
    });
}

#[test]
fn preloaded_resume_goal_pause_updates_persisted_goal_before_live_session_exists() {
    with_orca_home(|_| {
        let session_id = "019f8a00-0000-7000-8000-000000000002";
        orca_runtime::goal_store::GoalStore::load_default()
            .unwrap()
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "resumed objective".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();

        let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
            session_id.to_string(),
        ))));
        let fixture = transcript(session_id);
        history::SessionWriter::start_from_meta(fixture.meta)
            .expect("create resumable paused Goal transcript");
        let restored =
            history::load_session(session_id).expect("load resumable paused Goal transcript");
        let preloaded = Arc::new(Mutex::new(Some(restored)));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalPause).unwrap();
        let event = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if matches!(&event, TuiEvent::SurfaceProjectionSynced(projection)
            if projection.current_goal.as_ref().is_some_and(|goal| {
                goal.status == orca_core::goal_types::ThreadGoalStatus::Paused
            })) {
                break event;
            }
            if let TuiEvent::Error(message) = event {
                panic!("unexpected Goal pause error: {message}");
            }
        };
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        match event {
            TuiEvent::SurfaceProjectionSynced(projection) => {
                let goal = projection.current_goal.expect("paused Goal projection");
                assert_eq!(goal.session_id, session_id);
                assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Paused);
            }
            other => panic!("expected paused Goal projection, got {other:?}"),
        }
        let reloaded = orca_runtime::goal_store::GoalStore::load_default()
            .unwrap()
            .project_thread_goal(session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.status,
            orca_core::goal_types::ThreadGoalStatus::Paused
        );
    });
}

#[test]
fn preloaded_goal_edit_and_clear_restore_the_runtime_surface_before_mutation() {
    with_orca_home(|_| {
        let session_id = "019f8a00-0000-7000-8000-000000000003";
        orca_runtime::goal_store::GoalStore::load_default()
            .unwrap()
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "original objective".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();
        let fixture = transcript(session_id);
        history::SessionWriter::start_from_meta(fixture.meta)
            .expect("create resumable editable Goal transcript");
        let restored =
            history::load_session(session_id).expect("load resumable editable Goal transcript");
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
            session_id.to_string(),
        ))));
        let preloaded = Arc::new(Mutex::new(Some(restored)));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::GoalEdit("edited objective".to_string().into()))
            .unwrap();
        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection
                        .current_goal
                        .as_ref()
                        .is_some_and(|goal| goal.objective == "edited objective")
                        && projection.goal_presentation
                            == Some(
                                crate::surface_projection::GoalProjectionPresentation::Updated,
                            ) =>
                {
                    break;
                }
                TuiEvent::Error(message) => panic!("unexpected Goal edit error: {message}"),
                _ => {}
            }
        }
        action_tx.send(UserAction::GoalClear).unwrap();
        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_none()
                        && projection.goal_presentation
                            == Some(
                                crate::surface_projection::GoalProjectionPresentation::Cleared,
                            ) =>
                {
                    break;
                }
                TuiEvent::Error(message) => panic!("unexpected Goal clear error: {message}"),
                _ => {}
            }
        }
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(
            orca_runtime::goal_store::GoalStore::load_default()
                .unwrap()
                .project_thread_goal(session_id)
                .unwrap()
                .is_none(),
            "typed clear must persist the Goal tombstone"
        );
    });
}

#[test]
fn active_goal_pause_bypasses_command_backlog_and_cancels_goal_run() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::GoalSet(
            "mock_stream_delay_ms 5000".to_string().into(),
        ));
        harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started."))
            });

        harness.send(UserAction::GoalPause);
        let deadline = Instant::now() + Duration::from_secs(2);
        let paused = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "active /goal pause stayed behind the running operation"
            );
            let event = harness
                .event_rx
                .recv_timeout(remaining)
                .expect("active goal pause update");
            match &event {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.as_ref().is_some_and(|goal| {
                        goal.status == orca_core::goal_types::ThreadGoalStatus::Paused
                    }) && projection.goal_presentation
                        == Some(crate::surface_projection::GoalProjectionPresentation::Updated) =>
                {
                    break event;
                }
                TuiEvent::Error(message) => {
                    panic!("unexpected active Goal pause error: {message}")
                }
                _ => {}
            }
        };

        assert!(matches!(paused, TuiEvent::SurfaceProjectionSynced(_)));
        harness.shutdown();
    });
}

#[test]
fn typed_goal_projection_creation_and_removal_use_committed_snapshots() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::GoalSet(
            "mock_stream_delay_ms 5000".to_string().into(),
        ));
        let created = harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_some()
                        && projection.goal_presentation
                            == Some(crate::surface_projection::GoalProjectionPresentation::Updated))
        });
        let TuiEvent::SurfaceProjectionSynced(created) = created else {
            unreachable!("predicate accepted only a surface projection")
        };
        let created_cursor = created.cursor.clone();
        assert_eq!(
            created
                .current_goal
                .as_ref()
                .map(|goal| goal.objective.as_str()),
            Some("mock_stream_delay_ms 5000")
        );

        harness.send(UserAction::GoalPause);
        harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
            if projection.current_goal.as_ref().is_some_and(|goal| {
                goal.status == orca_core::goal_types::ThreadGoalStatus::Paused
            }))
        });
        harness.send(UserAction::GoalClear);
        let removed = harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_none()
                        && projection.goal_presentation
                            == Some(crate::surface_projection::GoalProjectionPresentation::Cleared))
        });
        let TuiEvent::SurfaceProjectionSynced(removed) = removed else {
            unreachable!("predicate accepted only a surface projection")
        };
        assert_eq!(removed.cursor.thread_id, created_cursor.thread_id);
        assert_eq!(removed.cursor.incarnation, created_cursor.incarnation);
        assert!(removed.cursor.next_seq > created_cursor.next_seq);

        harness.shutdown();
    });
}

#[test]
fn hosted_goal_set_materializes_active_paste_and_retains_file() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        let placeholder = "[Pasted Content 1001 chars]".to_string();
        let pasted = "x".repeat(1001);
        harness.send(UserAction::GoalSet(crate::protocol::GoalDraft {
            objective: format!("Use {placeholder}"),
            pending_pastes: vec![(placeholder, pasted.clone())],
        }));

        let created = harness.recv_until(|event| {
            matches!(event, TuiEvent::SurfaceProjectionSynced(projection)
                    if projection.current_goal.is_some())
        });
        let TuiEvent::SurfaceProjectionSynced(created) = created else {
            unreachable!("predicate accepted only a surface projection")
        };
        let objective = &created.current_goal.expect("created Goal").objective;
        let path = objective
            .strip_prefix("Use pasted text file: ")
            .and_then(|value| value.strip_suffix(". Read this file before continuing."))
            .expect("materialized paste reference");
        assert_eq!(std::fs::read_to_string(path).unwrap(), pasted);

        harness.shutdown();
        assert!(std::path::Path::new(path).exists());
    });
}

#[test]
fn queued_goal_set_preserves_immediate_interrupt_until_typed_operation_binds() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::GoalSet(
            "mock_stream_delay_ms 5000".to_string().into(),
        ));
        harness.send(UserAction::Interrupt);

        let terminal = harness.recv_until(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "cancelled"),
        );
        assert!(matches!(
            terminal,
            TuiEvent::SessionCompleted { status } if status == "cancelled"
        ));
        assert_eq!(harness.runtime.controller().current_id(), None);
        assert!(!harness.runtime.controller().has_surface_active());
        harness.shutdown();
    });
}

#[test]
fn preloaded_resume_goal_show_reads_persisted_goal_before_live_session_exists() {
    with_orca_home(|_| {
        let session_id = "resume-goal-show-session";
        orca_runtime::goal_store::GoalStore::load_default()
            .unwrap()
            .create_goal(orca_runtime::goal_store::CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "show resumed objective".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();

        let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
            session_id.to_string(),
        ))));
        let preloaded = Arc::new(Mutex::new(Some(transcript(session_id))));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalShow).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        match event {
            TuiEvent::GoalStatus(Some(goal)) => {
                assert_eq!(goal.session_id, session_id);
                assert_eq!(goal.objective, "show resumed objective");
                assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
            }
            other => panic!("expected resumed goal status, got {other:?}"),
        }
    });
}

#[test]
fn disabled_history_goal_show_still_reports_recorded_history_requirement() {
    let config = Arc::new(Mutex::new(test_config(HistoryMode::Disabled)));
    let preloaded = Arc::new(Mutex::new(None));
    let (event_tx, event_rx) = mpsc::unbounded();
    let (action_tx, action_rx) = mpsc::unbounded();
    let cancel = CancelToken::new();

    let handle = std::thread::spawn({
        let config = Arc::clone(&config);
        let preloaded = Arc::clone(&preloaded);
        let cancel = cancel.clone();
        move || {
            run_hosted_tui_controller_for_test(
                config,
                preloaded,
                event_tx,
                action_rx,
                cancel,
                test_pending_workflow_notifications(),
            )
        }
    });

    action_tx.send(UserAction::GoalShow).unwrap();
    let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    action_tx.send(UserAction::Cancel).unwrap();
    handle.join().unwrap();

    match event {
        TuiEvent::Error(message) => {
            assert_eq!(
                message,
                "persistent goals require recorded history; enable history before using /goal"
            );
        }
        other => panic!("expected recorded-history error, got {other:?}"),
    }
}

#[test]
fn backgrounded_hosted_tui_accepts_next_submit_before_first_turn_completes() {
    with_orca_home(|home| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let release_marker = home.join("release-backgrounded-turn");

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(format!(
                "mock_stream_release_marker {}",
                release_marker.display()
            )))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text)
                    if text.contains("Mock release-marker stream started.") =>
                {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();
        let backgrounded_task = loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("backgrounded task update");
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.status == orca_core::task_types::TaskStatus::Running
                    && task.is_backgrounded
            }) {
                break task;
            }
        };
        action_tx
            .send(UserAction::Submit("mock_history_echo".to_string()))
            .unwrap();

        let first_followup = loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                    break "next-submit";
                }
                TuiEvent::MessageDelta(text)
                    if text.contains("Mock release-marker stream completed.") =>
                {
                    break "first-turn-completed";
                }
                _ => {}
            }
        };
        assert_eq!(
            first_followup, "next-submit",
            "backgrounding must let the next foreground submit run before the backgrounded turn finishes"
        );
        std::fs::write(&release_marker, "release").expect("release backgrounded turn");
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("backgrounded task completion");
            if matching_task_update(event, |task| {
                task.id == backgrounded_task.id
                    && task.status == orca_core::task_types::TaskStatus::Completed
            })
            .is_some()
            {
                break;
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();
    });
}

#[test]
fn cancelled_hosted_tui_turn_does_not_cancel_next_submit() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 1000".to_string()));

        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started.") => {
                    assert!(harness.runtime.controller().has_surface_active());
                    break;
                }
                TuiEvent::Error(message) => panic!("unexpected first-turn error: {message}"),
                _ => {}
            }
        }

        harness.send(UserAction::Interrupt);
        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                TuiEvent::SessionCompleted { status } => {
                    assert_eq!(status, "cancelled");
                    assert!(!harness.runtime.controller().has_surface_active());
                    break;
                }
                TuiEvent::Error(message) => panic!("unexpected cancellation error: {message}"),
                _ => {}
            }
        }

        harness.send(UserAction::Submit("mock_history_echo".to_string()));

        let mut saw_second_start = false;
        let mut saw_second_output = false;
        loop {
            match harness
                .event_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                TuiEvent::TurnStarted { .. } => {
                    assert!(harness.runtime.controller().has_surface_active());
                    saw_second_start = true;
                }
                TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                    saw_second_output = true;
                }
                TuiEvent::SessionCompleted { status } => {
                    assert_eq!(status, "success");
                    break;
                }
                TuiEvent::Error(message) => panic!("unexpected second-turn error: {message}"),
                _ => {}
            }
        }

        harness.shutdown();

        assert!(saw_second_start, "second turn must start a fresh operation");
        assert!(saw_second_output, "second turn must run to provider output");
    });
}

#[test]
fn workflow_notification_submit_bypasses_user_file_mention_expansion() {
    with_orca_home(|_| {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(temp.path().join("outside.txt"), "outside").unwrap();

        let mut cfg = test_config(HistoryMode::Record);
        cfg.cwd = Some(workspace);
        let config = Arc::new(Mutex::new(cfg));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::SubmitWorkflowNotification(
                crate::protocol::PendingWorkflowNotification {
                    id: "notification-1".to_string(),
                    prompt: "mock_history_echo\nread @../outside.txt".to_string(),
                },
            ))
            .unwrap();

        let mut saw_history_echo = false;
        let mut unexpected_error = None;
        for _ in 0..10 {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                    saw_history_echo = true;
                    break;
                }
                TuiEvent::Error(message) => {
                    unexpected_error = Some(message);
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert_eq!(unexpected_error, None);
        assert!(
            saw_history_echo,
            "workflow notifications should not be preprocessed as user-authored @file mentions"
        );
    });
}

#[test]
fn workflow_notification_submit_uses_one_typed_turn_with_notification_label() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::SubmitWorkflowNotification(
                crate::protocol::PendingWorkflowNotification {
                    id: "notification-1".to_string(),
                    prompt: "<task-notification>mock_history_echo</task-notification>".to_string(),
                },
            ))
            .unwrap();

        let mut task_id = None;
        let mut title = None;
        loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match event {
                TuiEvent::TurnStarted {
                    task: Some(task), ..
                } => {
                    task_id = Some(task.id);
                }
                TuiEvent::SurfaceProjectionSynced(projection) => title = Some(projection.title),
                TuiEvent::SessionCompleted { .. } => break,
                _ => {}
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(
            task_id.is_some(),
            "typed surface must publish the turn task"
        );
        assert_eq!(
            title.as_deref(),
            Some("Workflow notification notification-1")
        );
    });
}

#[test]
fn workflow_notification_first_turn_uses_notification_label_for_session_title() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::SubmitWorkflowNotification(
                crate::protocol::PendingWorkflowNotification {
                    id: "notification-1".to_string(),
                    prompt: "<task-notification>mock_history_echo</task-notification>".to_string(),
                },
            ))
            .unwrap();

        loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if matches!(
                event,
                TuiEvent::SurfaceProjectionSynced(ref projection)
                    if projection.title == "Workflow notification notification-1"
            ) {
                break;
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        let transcript = history::load_session("latest").expect("latest session");
        assert_eq!(
            transcript.meta.title,
            "Workflow notification notification-1"
        );
        assert!(!transcript.meta.title.contains("<task-notification>"));
    });
}

#[test]
fn hosted_user_turn_request_opts_into_task_tracking_without_goal_tools() {
    let submitted = SubmittedTurn::user("inspect the runtime".to_string());

    let request = hosted_turn_request(&submitted, false);

    assert!(!request.allows_goal_tools());
    assert!(!request.tracks_goal_usage());
    assert!(request.is_backtrack_target());
    assert_eq!(request.task_description(), Some("inspect the runtime"));
}

#[test]
fn hosted_goal_notification_request_preserves_pinned_task_semantics() {
    let submitted =
        SubmittedTurn::workflow_notification(crate::protocol::PendingWorkflowNotification {
            id: "notification-42".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
        });

    let request = hosted_turn_request(&submitted, true);

    assert!(request.allows_goal_tools());
    assert!(request.tracks_goal_usage());
    assert!(!request.is_backtrack_target());
    assert_eq!(
        request.task_description(),
        Some("Workflow notification notification-42")
    );
}

#[test]
fn backgrounded_hosted_tui_does_not_complete_unexecuted_tool_calls() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let status = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status != orca_core::task_types::TaskStatus::Running
            }) {
                break task.status;
            }
        };

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert_ne!(
            status,
            orca_core::task_types::TaskStatus::Completed,
            "background completion must not report success for tool calls that were not executed"
        );
    });
}

#[test]
fn backgrounded_hosted_tui_marks_unexecuted_tool_calls_approval_required() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let status = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status != orca_core::task_types::TaskStatus::Running
            }) {
                break task.status;
            }
        };

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!("approval_required"),
            "backgrounded turns that stop before executing tool calls must be actionable"
        );
    });
}

#[test]
fn backgrounded_hosted_tui_reports_pending_tool_name() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let pending_tool = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
            }) {
                break task.pending_tool_call;
            }
        };

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        let pending_tool = pending_tool.expect("pending tool call");
        assert_eq!(pending_tool.id, "mock-tool-1");
        assert_eq!(pending_tool.name, "task_list");
        assert_eq!(
            pending_tool.action,
            orca_core::approval_types::ActionKind::Read
        );
        assert_eq!(pending_tool.arguments, "{}");
    });
}

#[test]
fn backgrounded_hosted_tui_notifies_approval_required_in_user_language() {
    with_orca_home(|home| {
        let mut cfg = test_config(HistoryMode::Record);
        cfg.cwd = Some(home.to_path_buf());
        let config = Arc::new(Mutex::new(cfg));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let mut notice = None;
        let mut seen = Vec::new();
        for _ in 0..20 {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::Notice(message) if message.starts_with("Background session") => {
                    notice = Some(message);
                    break;
                }
                TuiEvent::Notice(message) => {
                    seen.push(format!("notice: {message}"));
                }
                TuiEvent::SurfaceProjectionSynced(projection) => {
                    let statuses = projection
                        .workflow_tasks
                        .into_iter()
                        .filter(|task| {
                            task.task_type == orca_core::task_types::TaskType::MainSession
                        })
                        .map(|task| format!("{:?}", task.status))
                        .collect::<Vec<_>>();
                    seen.push(format!("tasks: {}", statuses.join(",")));
                }
                event => seen.push(format!("{event:?}")),
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert_eq!(
            notice.unwrap_or_else(|| panic!("missing background notice; saw {seen:?}")),
            "Background session needs approval for task_list before it can continue."
        );
    });
}

#[test]
fn approved_background_tool_call_executes_and_completes_session() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 task_list".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let (task_id, approval_id) = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
            }) {
                let approval_id = task
                    .pending_tool_call
                    .as_ref()
                    .expect("pending tool call")
                    .id
                    .clone();
                break (task.id, approval_id);
            }
        };

        action_tx
            .send(UserAction::ResolveBackgroundApproval {
                id: approval_id,
                approved: true,
            })
            .unwrap();

        let mut saw_completion_message = false;
        let mut saw_completed_task = false;
        let mut saw_output_handoff = false;
        let mut seen = Vec::new();
        for _ in 0..40 {
            match event_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(TuiEvent::MessageDelta(text)) => {
                    if text.contains("Mock completed after tool execution.") {
                        saw_completion_message = true;
                    }
                    seen.push(format!("delta: {text}"));
                }
                Ok(TuiEvent::SurfaceProjectionSynced(projection)) => {
                    saw_completed_task |= projection.workflow_tasks.into_iter().any(|task| {
                        task.id == task_id
                            && task.status == orca_core::task_types::TaskStatus::Completed
                    });
                }
                Ok(TuiEvent::BackgroundTaskOutputAttached { task_id: attached })
                    if attached == task_id =>
                {
                    saw_output_handoff = true;
                }
                Ok(event) => seen.push(format!("{event:?}")),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    seen.push("timeout".to_string());
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("agent event channel disconnected before background continuation")
                }
            }
            if saw_completion_message && saw_completed_task && saw_output_handoff {
                break;
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(
            saw_completion_message,
            "approved background tool call should continue the model loop; saw {seen:?}"
        );
        assert!(
            saw_completed_task,
            "approved background tool call should complete the background task; saw {seen:?}"
        );
        assert!(
            saw_output_handoff,
            "approved background tool call should hydrate its durable background output; saw {seen:?}"
        );
    });
}

#[test]
fn approved_background_tool_call_does_not_prompt_again_for_same_tool() {
    with_orca_home(|_| {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                run_hosted_tui_controller_for_test(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx
            .send(UserAction::Submit(
                "mock_stream_tool_delay_ms 250 mcp__broken__tool".to_string(),
            ))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                TuiEvent::MessageDelta(text) if text.contains("Mock slow tool stream started.") => {
                    break;
                }
                _ => {}
            }
        }

        action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

        let approval_id = loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(task) = matching_task_update(event, |task| {
                task.task_type == orca_core::task_types::TaskType::MainSession
                    && task.is_backgrounded
                    && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                    && task
                        .pending_tool_call
                        .as_ref()
                        .is_some_and(|tool| tool.name == "mcp__broken__tool")
            }) {
                break task
                    .pending_tool_call
                    .as_ref()
                    .expect("pending tool call")
                    .id
                    .clone();
            }
        };

        action_tx
            .send(UserAction::ResolveBackgroundApproval {
                id: approval_id,
                approved: true,
            })
            .unwrap();

        let mut saw_tool_execution = false;
        let mut saw_second_approval = false;
        let mut seen = Vec::new();
        for _ in 0..20 {
            match event_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(TuiEvent::ToolRequested { name, .. }) if name == "mcp__broken__tool" => {
                    saw_tool_execution = true;
                    break;
                }
                Ok(TuiEvent::ToolCompleted { name, .. }) if name == "mcp__broken__tool" => {
                    saw_tool_execution = true;
                    break;
                }
                Ok(TuiEvent::ApprovalNeeded { key, tool, .. }) => {
                    saw_second_approval = true;
                    seen.push(format!("approval: {tool}"));
                    action_tx
                        .send(UserAction::RespondToInteraction {
                            key,
                            response: TuiInteractionResponse::Approval(false),
                        })
                        .unwrap();
                    break;
                }
                Ok(event) => seen.push(format!("{event:?}")),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    seen.push("timeout".to_string());
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("agent event channel disconnected before background tool execution")
                }
            }
        }

        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(
            saw_tool_execution,
            "approved background tool should execute without a second approval; saw {seen:?}"
        );
        assert!(
            !saw_second_approval,
            "approved background tool should not prompt again for the same call"
        );
    });
}

#[test]
fn idle_app_submits_pending_workflow_notification() {
    let (mut state, _rx) = test_state();
    let (action_tx, action_rx) = mpsc::unbounded();
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
        });

    submit_pending_workflow_notification(&mut state, &action_tx, true);

    assert_eq!(state.status, AppStatus::Running);
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWorkflowNotification(notification))
            if notification.id == "notification-1"
                && notification.prompt == "<task-notification>done</task-notification>"
    ));
}

#[test]
fn tool_completion_is_not_a_workflow_notification_turn_boundary() {
    assert!(!is_workflow_notification_turn_boundary(
        &TuiEvent::ToolCompleted {
            id: "tool-1".to_string(),
            name: "bash".to_string(),
            status: "completed".to_string(),
            output: String::new(),
            diff: None,
            kind: None,
        }
    ));
    assert!(!is_workflow_notification_turn_boundary(
        &TuiEvent::SubagentCompleted {
            id: "agent-1".to_string(),
            description: "inspect".to_string(),
            status: "success".to_string(),
            output: None,
            error: None,
        }
    ));
}

#[test]
fn session_completion_submits_pending_workflow_notification() {
    let (mut state, _rx) = test_state();
    let (action_tx, action_rx) = mpsc::unbounded();
    state.status = AppStatus::Running;
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>failed</task-notification>".to_string(),
        });

    assert!(is_workflow_notification_turn_boundary(
        &TuiEvent::SessionCompleted {
            status: "success".to_string(),
        }
    ));
    submit_pending_workflow_notification(&mut state, &action_tx, false);

    assert_eq!(state.status, AppStatus::Running);
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWorkflowNotification(notification))
            if notification.id == "notification-1"
                && notification.prompt == "<task-notification>failed</task-notification>"
    ));
}

#[test]
fn session_completion_drains_batch_boundary_queue_before_submitting_notification() {
    let (mut state, _rx) = test_state();
    let (action_tx, action_rx) = mpsc::unbounded();
    let queue = test_pending_workflow_notifications();
    assert!(
        queue.push_unique(crate::protocol::PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>failed</task-notification>".to_string(),
        })
    );
    state.status = AppStatus::Running;

    drain_pending_workflow_notifications(&mut state, &queue);
    submit_pending_workflow_notification(&mut state, &action_tx, false);

    assert!(queue.is_empty());
    assert!(state.pending_workflow_notifications.is_empty());
    assert_eq!(state.status, AppStatus::Running);
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWorkflowNotification(notification))
            if notification.id == "notification-1"
                && notification.prompt == "<task-notification>failed</task-notification>"
    ));
}

#[test]
fn terminal_workflow_notifications_enter_batch_boundary_queue() {
    let queue = test_pending_workflow_notifications();
    let queued = queue_workflow_terminal_notification(
        &TuiEvent::WorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "done".to_string(),
        },
        &queue,
        true,
    );
    assert_eq!(queued.as_deref(), Some("notification-1"));
    let notification = queue.pop_front().expect("notification");
    assert_eq!(notification.id, "notification-1");
    assert_eq!(
        notification.prompt,
        "<task-notification>done</task-notification>"
    );

    let queued = queue_workflow_terminal_notification(
        &TuiEvent::WorkflowNotification {
            id: "notification-2".to_string(),
            prompt: "<task-notification>failed</task-notification>".to_string(),
            status: "failed".to_string(),
            summary: "failed".to_string(),
        },
        &queue,
        true,
    );
    assert_eq!(queued.as_deref(), Some("notification-2"));
    let notification = queue.pop_front().expect("notification");
    assert_eq!(notification.id, "notification-2");
    assert_eq!(
        notification.prompt,
        "<task-notification>failed</task-notification>"
    );

    let queued = queue_workflow_terminal_notification(
        &TuiEvent::WorkflowNotification {
            id: "notification-3".to_string(),
            prompt: "<task-notification>failed</task-notification>".to_string(),
            status: "failed".to_string(),
            summary: "failed".to_string(),
        },
        &queue,
        false,
    );
    assert!(queued.is_none());
    assert!(queue.is_empty());
}

#[test]
fn terminal_workflow_notifications_skip_duplicate_batch_queue_id() {
    let queue = test_pending_workflow_notifications();
    let event = TuiEvent::WorkflowNotification {
        id: "notification-1".to_string(),
        prompt: "<task-notification>done</task-notification>".to_string(),
        status: "completed".to_string(),
        summary: "done".to_string(),
    };

    assert_eq!(
        queue_workflow_terminal_notification(&event, &queue, true).as_deref(),
        Some("notification-1")
    );
    assert!(queue_workflow_terminal_notification(&event, &queue, true).is_none());
    assert_eq!(queue.len(), 1);
}

#[test]
fn batch_queued_workflow_notification_is_removed_from_ui_pending_queue_by_id() {
    let (mut state, _rx) = test_state();
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>completed</task-notification>".to_string(),
        });
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "notification-2".to_string(),
            prompt: "<task-notification>failed</task-notification>".to_string(),
        });

    remove_pending_workflow_notification_by_id(&mut state, "notification-2");

    assert_eq!(
        state
            .pending_workflow_notifications
            .iter()
            .map(|notification| notification.prompt.as_str())
            .collect::<Vec<_>>(),
        vec!["<task-notification>completed</task-notification>"]
    );
}

#[test]
fn batch_queued_workflow_notification_removal_uses_notification_id() {
    let (mut state, _rx) = test_state();
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "workflow-run-1:tool-use-1".to_string(),
            prompt: "<task-notification>same</task-notification>".to_string(),
        });
    state
        .pending_workflow_notifications
        .push_back(crate::protocol::PendingWorkflowNotification {
            id: "workflow-run-2:tool-use-2".to_string(),
            prompt: "<task-notification>same</task-notification>".to_string(),
        });

    remove_pending_workflow_notification_by_id(&mut state, "workflow-run-2:tool-use-2");

    let pending = state
        .pending_workflow_notifications
        .iter()
        .map(|notification| notification.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(pending, vec!["workflow-run-1:tool-use-1"]);
}

#[test]
fn settled_messages_remain_in_fullscreen_transcript_after_turn_end() {
    let theme = Theme::named(ThemeName::Dark);
    let (tx, _rx) = mpsc::unbounded();
    let mut state = AppState::new(
        tx,
        "0.0.0-test".to_string(),
        "auto".to_string(),
        "/tmp".to_string(),
    );
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state
        .transcript
        .messages
        .push(ChatMessage::Assistant("answer".to_string()));
    state.transcript.finalized_count = state.transcript.messages.len();
    state.status = AppStatus::Idle;

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("test backend");

    terminal
        .draw(|frame| ui::render(frame, &mut state, &TextArea::default(), &theme))
        .expect("draw");

    assert_eq!(state.transcript.flushed_count, 0);
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("hi"));
    assert!(rendered.contains("answer"));
}

#[test]
#[ignore = "superseded by runtime-owned prompt queue integration tests"]
fn running_queue_preview_restore_and_terminal_dispatch_frames_are_consistent() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let mut config = test_config(HistoryMode::Record);
    let shared = Arc::new(Mutex::new(config.clone()));
    let preloaded = Arc::new(Mutex::new(None));
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();

    for code in [KeyCode::Char('f'), KeyCode::Char('o'), KeyCode::Char('o')] {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();
    }
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_status_key(
        &Event::Key(enter),
        &enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(state.queued_pending_visible_text().len(), 1);
    assert!(action_rx.try_recv().is_err());
    assert!(
        !state
            .transcript
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::User(text) if text == "foo"))
    );

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("Queued 1"));

    let restore = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    handle_status_key(
        &Event::Key(restore),
        &restore,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert!(state.queued_pending_visible_text().is_empty());
    assert_eq!(textarea_text(&textarea), "foo");

    textarea.insert_char('!');
    handle_status_key(
        &Event::Key(enter),
        &enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert!(action_rx.try_recv().is_err());

    let pending = test_pending_workflow_notifications();
    let mut presentation = TerminalPresentation::new(
        false,
        TerminalPresentationProfile {
            osc9_supported: false,
            tmux_passthrough: false,
        },
    );
    handle_runtime_event(
        TuiEvent::SessionCompleted {
            status: "success".to_string(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "foo!"
    ));
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::User(text)) if text == "foo!"
    ));
    assert!(state.queued_pending_visible_text().is_empty());
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .unwrap();
    assert!(!format!("{:?}", terminal.backend().buffer()).contains("Queued 1"));
}

#[test]
#[ignore = "superseded by runtime-owned prompt queue integration tests"]
fn hosted_tui_runs_app_state_queued_follow_ups_one_at_a_time_in_fifo_order() {
    with_orca_home(|_| {
        let mut harness = HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit("mock_stream_delay_ms 100".to_string()));
        harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
        let first_terminal = harness.recv_until(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success"),
        );

        let mut state = AppState::new(
            harness.action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        for _ in 0..2 {
            state
                .enqueue_user_message(
                    crate::queued_input::QueuedUserMessage::from_composer(
                        "mock_history_echo".to_string(),
                        Vec::new(),
                        orca_runtime::mentions::MentionBindings::default(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        state.enter_running();
        let pending = test_pending_workflow_notifications();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = TerminalPresentation::new(
            false,
            TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        );

        let mut terminal_event = first_terminal;
        for expected_count in [2usize, 3usize] {
            handle_runtime_event(
                terminal_event,
                &mut state,
                &harness.action_tx,
                &pending,
                &mut textarea,
                &mut vim,
                &theme,
                &mut presentation,
            );
            let queued_started = harness
                .recv_until(|event| matches!(event, TuiEvent::QueuedSubmissionStarted { .. }));
            state.update(queued_started);
            let turn_started =
                harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
            state.update(turn_started);
            let delta = harness.recv_until(|event| {
                matches!(
                    event,
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock history users:")
                )
            });
            let TuiEvent::MessageDelta(text) = delta else {
                unreachable!()
            };
            let expected = format!(
                "Mock history users: {}",
                std::iter::once("mock_stream_delay_ms 100")
                    .chain(std::iter::repeat_n("mock_history_echo", expected_count - 1,))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            assert_eq!(text, expected);
            terminal_event = harness.recv_until(|event| {
                matches!(
                    event,
                    TuiEvent::SessionCompleted { status }
                        if status == "success"
                )
            });
            if expected_count == 2 {
                assert_eq!(state.queued_pending_visible_text().len(), 1);
                state.set_status(AppStatus::Running);
            }
        }

        assert!(state.queued_pending_visible_text().is_empty());
        harness.shutdown();
    });
}

#[test]
fn search_keyboard_frames_move_active_match_without_composer_mutation() {
    let (mut state, _rx) = test_state();
    for index in 0..30 {
        state.push_message(ChatMessage::System(format!("row {index:02} alpha")));
    }
    state.viewport.auto_scroll = false;
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::from(["composer draft"]);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("test backend");
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("initial draw");

    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("search draw");
    let first = state.transcript.search.active_ordinal();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("1/30"));

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
    );
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("next draw");
    assert_ne!(state.transcript.search.active_ordinal(), first);
    assert!(!state.viewport.auto_scroll);
    assert_eq!(textarea.lines(), &["composer draft".to_string()]);

    handle_transcript_search_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut state,
    );
    assert_eq!(state.transcript.search.active_ordinal(), first);
}

#[test]
fn running_search_esc_closes_before_interrupt_and_paste_never_touches_composer() {
    let (mut state, _state_action_rx) = test_state();
    state.enter_running();
    state.open_transcript_search();
    let mut textarea = TextArea::from(["composer"]);
    let mut config = test_config(HistoryMode::Record);
    let shared = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut vim = VimState::new(false);

    assert!(handle_paste_event(
        &Event::Paste("alpha\r\nbeta".to_string()),
        &mut state,
        &config,
        &action_tx,
        &mut textarea,
    ));
    assert_eq!(state.transcript.search.query(), "alpha beta");
    assert_eq!(textarea.lines(), &["composer".to_string()]);
    assert!(state.pending_pastes.is_empty());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key_event_preflight(
        esc,
        &mut state,
        &mut config,
        &action_tx,
        &mut vim,
        true,
        || Ok(()),
    )
    .unwrap();
    assert!(!state.transcript.search.open);

    let preloaded = Arc::new(Mutex::new(None));
    let theme = Theme::named(ThemeName::Dark);
    handle_status_key(
        &Event::Key(esc),
        &esc,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
}

#[test]
fn mouse_selection_over_search_match_wins_and_copy_stays_exact() {
    let (mut state, _rx) = test_state();
    state.push_message(ChatMessage::System("alpha beta".to_string()));
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 8)).expect("test backend");
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("search draw");

    state.viewport.selection = Some(TranscriptSelection::unit(
        SelectionGranularity::Cell,
        SelectionPos { row: 0, col: 1 },
        SelectionPos { row: 0, col: 3 },
    ));
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("selection draw");
    assert_eq!(
        state
            .transcript
            .render_cache
            .extract_text(state.viewport.selection.as_ref().unwrap()),
        "lph"
    );
    let selected_cells = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| cell.style().bg == theme.selection_style().bg)
        .count();
    assert!(selected_cells >= 3);
}

#[test]
fn streaming_and_resize_refresh_matches_without_stealing_active_identity() {
    let (mut state, _rx) = test_state();
    state.update(TuiEvent::MessageDelta(
        "prefix long words before alpha\n\nhidden alpha".to_string(),
    ));
    state.open_transcript_search();
    state.replace_transcript_search_query("alpha");
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::default();
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 8)).expect("test backend");
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("held draw");
    assert_eq!(state.transcript.search.match_count(), 1);
    let identity = state
        .transcript
        .search
        .active_match()
        .unwrap()
        .line_identity;

    state.update(TuiEvent::MessageDelta("\n".to_string()));
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("released draw");
    assert_eq!(state.transcript.search.match_count(), 2);
    assert_eq!(
        state
            .transcript
            .search
            .active_match()
            .unwrap()
            .line_identity,
        identity
    );
    let before = state.transcript.search.active_match().unwrap().start;

    let mut resized =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(8, 8)).expect("resized backend");
    resized
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .expect("resized draw");
    assert_eq!(
        state
            .transcript
            .search
            .active_match()
            .unwrap()
            .line_identity,
        identity
    );
    assert_ne!(
        state.transcript.search.active_match().unwrap().start,
        before
    );
}

#[test]
#[ignore = "legacy local queue admission shim removed"]
fn slash_menu_tab_opens_resume_picker_like_enter() {
    with_orca_home(|home| {
        orca_runtime::history::SessionWriter::start(
            home,
            "mock",
            Some("auto".to_string()),
            "history tab test",
        )
        .unwrap();

        let (mut state, _rx) = test_state();
        state
            .enqueue_user_message(
                crate::queued_input::QueuedUserMessage::from_composer(
                    "in flight".to_string(),
                    Vec::new(),
                    orca_runtime::mentions::MentionBindings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        state.set_status(AppStatus::Idle);
        state
            .begin_next_queued_message()
            .expect("seed in-flight queued submission");
        state
            .enqueue_user_message(
                crate::queued_input::QueuedUserMessage::from_composer(
                    "queued".to_string(),
                    Vec::new(),
                    orca_runtime::mentions::MentionBindings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        state.report_queued_input_error("error".to_string());
        state.suspend_queued_follow_up_autosend();
        state.set_status(AppStatus::Idle);
        state.slash_menu = Some(SlashMenu {
            items: commands::all_commands()
                .iter()
                .map(|(command, description)| SlashMenuItem {
                    command: (*command).to_string(),
                    description: (*description).to_string(),
                })
                .collect(),
            selected: commands::all_commands()
                .iter()
                .position(|(command, _)| *command == "/resume")
                .unwrap(),
            sub_menu: None,
        });
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _action_rx) = mpsc::unbounded();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);
        let vim_state = VimState::new(false);
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ));
        let key = match &event {
            Event::Key(key) => key,
            _ => unreachable!(),
        };

        assert!(crate::slash_menu_actions::handle_slash_menu_key(
            &event,
            key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(state.status, AppStatus::SessionPicker);
        assert!(!state.session_picker_sessions.is_empty());
        assert!(state.slash_menu.is_none());
        assert!(state.queued_pending_visible_text().is_empty());
        assert!(!state.queued_submission_in_flight());
        assert!(state.queued_input_error().is_none());
        assert!(state.queued_autosend_enabled());
    });
}

#[test]
fn slash_menu_tab_completes_goal_objective_prefix_without_dispatching() {
    let (mut state, _rx) = test_state();
    state.status = AppStatus::Idle;
    state.slash_menu = Some(SlashMenu {
        items: commands::all_commands()
            .iter()
            .map(|(command, description)| SlashMenuItem {
                command: (*command).to_string(),
                description: (*description).to_string(),
            })
            .collect(),
        selected: commands::all_commands()
            .iter()
            .position(|(command, _)| *command == "/goal")
            .unwrap(),
        sub_menu: None,
    });
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = make_textarea(&VimState::new(false), &theme);
    let vim_state = VimState::new(false);
    let event = Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    ));
    let key = match &event {
        Event::Key(key) => key,
        _ => unreachable!(),
    };

    assert!(crate::slash_menu_actions::handle_slash_menu_key(
        &event,
        key,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
        &mut textarea,
        &vim_state,
        &theme,
    ));

    assert_eq!(textarea_text(&textarea), "/goal ");
    assert_eq!(state.status, AppStatus::Idle);
    assert!(state.slash_menu.is_none());
    assert!(action_rx.try_recv().is_err());
}

#[test]
fn slash_submenu_model_flow_asks_for_reasoning_effort_then_applies_both() {
    let (mut state, _rx) = test_state();
    state.slash_menu = Some(SlashMenu {
        items: Vec::new(),
        selected: 0,
        sub_menu: Some(SubMenu {
            title: "/model".to_string(),
            items: vec!["deepseek-v4-pro".to_string()],
            selected: 0,
            context: None,
        }),
    });
    let mut config = test_config(HistoryMode::Record);
    config.reasoning_effort = orca_core::config::ReasoningEffort::Max;
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = make_textarea(&VimState::new(false), &theme);
    let vim_state = VimState::new(false);

    let press = |key_code: KeyCode,
                 state: &mut AppState,
                 config: &mut RunConfig,
                 textarea: &mut TextArea| {
        let event = Event::Key(crossterm::event::KeyEvent::new(
            key_code,
            crossterm::event::KeyModifiers::NONE,
        ));
        let key = match &event {
            Event::Key(key) => *key,
            _ => unreachable!(),
        };
        assert!(crate::slash_menu_actions::handle_slash_menu_key(
            &event,
            &key,
            state,
            config,
            &shared_config,
            &action_tx,
            textarea,
            &vim_state,
            &theme,
        ));
    };

    // Step 1: picking a model must NOT apply anything yet — it opens the
    // reasoning-effort picker, pre-selected on the current effort (max).
    press(KeyCode::Tab, &mut state, &mut config, &mut textarea);
    let sub = state
        .slash_menu
        .as_ref()
        .and_then(|menu| menu.sub_menu.as_ref())
        .expect("reasoning submenu should open");
    assert_eq!(
        sub.title,
        crate::slash_menu_actions::REASONING_SUBMENU_TITLE
    );
    assert_eq!(sub.context.as_deref(), Some("deepseek-v4-pro"));
    assert!(sub.items[sub.selected].starts_with("max"));
    assert_eq!(state.model_name, "auto", "not applied yet");

    // Step 2: pick "high" (first item), applying model + effort together.
    press(KeyCode::Up, &mut state, &mut config, &mut textarea);
    press(KeyCode::Enter, &mut state, &mut config, &mut textarea);

    assert_eq!(
        state.model_name, "auto",
        "not applied before runtime commit"
    );
    assert_eq!(
        state.reasoning_effort,
        orca_core::config::ReasoningEffort::Max,
        "not applied before runtime commit"
    );
    assert_eq!(config.model.display_name(), "auto");
    assert_eq!(
        config.reasoning_effort,
        orca_core::config::ReasoningEffort::Max
    );
    let shared = shared_config.lock().unwrap();
    assert_eq!(shared.model.display_name(), "auto");
    assert_eq!(
        shared.reasoning_effort,
        orca_core::config::ReasoningEffort::Max
    );
    drop(shared);
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SetModel(intent))
            if intent == "__orca_runtime_settings__:deepseek-v4-pro|high|-"
    ));
    assert!(state.slash_menu.is_none());
}

#[test]
fn workflow_slash_command_dispatches_structured_run_action() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();

    handle_slash_command(
        "/workflow:security-audit target=src maxAgents=8",
        &mut config,
        &shared_config,
        &mut state,
        &action_tx,
    );

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::RunWorkflow { name, args })
            if name == "security-audit" && args.as_deref() == Some("target=src maxAgents=8")
    ));
}

#[test]
fn bracketed_paste_inserts_multiline_text_without_submitting() {
    let (_state, _rx) = test_state();
    let (_action_tx, action_rx) = mpsc::unbounded::<UserAction>();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = make_textarea(&VimState::new(false), &theme);

    assert!(insert_pasted_text(&mut textarea, "alpha\nbravo\ncharlie"));

    assert_eq!(textarea_text(&textarea), "alpha\nbravo\ncharlie");
    assert!(action_rx.try_recv().is_err());
}

#[test]
fn bracketed_paste_can_insert_newline_after_existing_text() {
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = make_textarea_with_text("prefix", &VimState::new(false), &theme);

    assert!(insert_pasted_text(&mut textarea, "\nnext"));

    assert_eq!(textarea_text(&textarea), "prefix\nnext");
}

#[test]
fn large_paste_submits_full_content_and_clears_pending_payload() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let mut textarea = make_textarea(&vim_state, &theme);
    let pasted = "long line\n".repeat(120);

    assert!(insert_composer_paste(
        &mut textarea,
        &mut state.pending_pastes,
        &pasted,
    ));
    assert!(textarea_text(&textarea).starts_with("[Pasted Content "));

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim_state,
        &theme,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
    ));

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions {
            prompt, bindings, ..
        })
            if prompt == pasted.trim() && bindings.is_empty()
    ));
    assert!(state.pending_pastes.is_empty());
    assert!(textarea_text(&textarea).is_empty());
    assert_eq!(state.input_history, vec![pasted.trim().to_string()]);
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::User(display)) if display.starts_with("[Pasted Content ")
    ));
}

#[test]
fn large_paste_rebases_atomic_mention_binding_before_submit() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let mut textarea = make_textarea(&vim_state, &theme);
    let pasted = "long line\n".repeat(120);
    let mention = "@same.txt";

    assert!(insert_composer_paste(
        &mut textarea,
        &mut state.pending_pastes,
        &pasted,
    ));
    assert!(textarea.insert_str(&format!(" review {mention}")));

    let visible_prompt = textarea_text(&textarea);
    let mention_start = visible_prompt.find(mention).expect("visible mention");
    state.mention_bindings = orca_runtime::mentions::MentionBindings::from_bindings(
        &visible_prompt,
        vec![orca_runtime::mentions::MentionBinding {
            start: mention_start,
            end: mention_start + mention.len(),
            visible: mention.to_string(),
            target: orca_runtime::mentions::MentionTarget::File {
                root: PathBuf::from("/workspace/backend"),
                path: "same.txt".to_string(),
                kind: orca_runtime::mentions::MentionFileKind::File,
            },
        }],
    );

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim_state,
        &theme,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
    ));

    let action = action_rx.try_recv().expect("submit action");
    let UserAction::SubmitWithMentions {
        prompt, bindings, ..
    } = action
    else {
        panic!("expected mention-aware submit");
    };
    assert_eq!(prompt, format!("{pasted} review {mention}"));
    assert_eq!(bindings.bindings().len(), 1);
    let binding = &bindings.bindings()[0];
    let rebased_start = prompt.find(mention).expect("expanded mention");
    assert_eq!(binding.start, rebased_start);
    assert_eq!(binding.end, rebased_start + mention.len());
    assert_eq!(binding.visible, mention);
}

#[test]
fn waiting_user_input_submit_sends_typed_user_input_response() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let mut textarea = make_textarea_with_text("continue", &vim_state, &theme);
    let key = interaction_key(TuiInteractionKind::UserInput, "ask-1");
    state.set_status(AppStatus::WaitingUserInput);
    state.interaction.pending_input = Some(PendingTuiInput::UserInput(key.clone()));

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim_state,
        &theme,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
    ));

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::RespondToInteraction {
            key: actual_key,
            response: TuiInteractionResponse::UserInput(answer),
        }) if actual_key == key && answer == "continue"
    ));
    assert!(state.interaction.pending_input.is_none());
    assert_eq!(state.status, AppStatus::Running);
}

#[test]
fn waiting_mcp_elicitation_submit_sends_typed_mcp_response() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let mut textarea = make_textarea_with_text(
        r#"{"repository":"echoVic/blade-deepseek"}"#,
        &vim_state,
        &theme,
    );
    let key = interaction_key(TuiInteractionKind::McpElicitation, "mcp-1");
    state.set_status(AppStatus::WaitingUserInput);
    state.interaction.pending_input = Some(PendingTuiInput::McpElicitation(key.clone()));

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim_state,
        &theme,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
    ));

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::RespondToInteraction {
            key: actual_key,
            response: TuiInteractionResponse::McpElicitation {
                accepted: true,
                content_json: Some(content),
            },
        }) if actual_key == key && content == r#"{"repository":"echoVic/blade-deepseek"}"#
    ));
    assert!(state.interaction.pending_input.is_none());
    assert_eq!(state.status, AppStatus::Running);
}

#[test]
fn repaired_indeterminate_history_tool_renders_state_inspection_warning() {
    let request = orca_core::tool_types::ToolRequest {
        id: "legacy-call".to_string(),
        name: orca_core::tool_types::ToolName::Bash,
        action: orca_core::approval_types::ActionKind::Shell,
        target: Some("deploy".to_string()),
        raw_arguments: None,
    };
    let result = orca_core::tool_types::ToolResult::indeterminate(
        &request,
        "legacy tool call has no terminal result",
    )
    .with_terminal_source(orca_core::tool_types::ToolTerminalSource::CompatibilityRepair);

    let message = chat_message_from_history(Message::Tool {
        tool_call_id: request.id,
        content: "legacy missing result".to_string(),
        terminal: Some(result.terminal().clone()),
        pinned: false,
    })
    .expect("history tool message");

    let ChatMessage::ToolCall {
        status,
        output,
        kind,
        ..
    } = message
    else {
        panic!("expected tool row")
    };
    assert_eq!(status, "indeterminate");
    assert_eq!(kind.as_deref(), Some("indeterminate"));
    assert!(
        output
            .as_deref()
            .is_some_and(|output| output.contains("Inspect external state before retrying"))
    );
}

#[test]
fn idle_submit_carries_atomic_mention_bindings() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let prompt = "review @same.txt";
    let mut textarea = make_textarea_with_text(prompt, &vim_state, &theme);
    state.mention_bindings = orca_runtime::mentions::MentionBindings::from_bindings(
        prompt,
        vec![orca_runtime::mentions::MentionBinding {
            start: 7,
            end: prompt.len(),
            visible: "@same.txt".to_string(),
            target: orca_runtime::mentions::MentionTarget::File {
                root: PathBuf::from("/workspace/backend"),
                path: "same.txt".to_string(),
                kind: orca_runtime::mentions::MentionFileKind::File,
            },
        }],
    );

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim_state,
        &theme,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
    ));

    let action = action_rx.try_recv().expect("submit action");
    let UserAction::SubmitWithMentions {
        prompt, bindings, ..
    } = action
    else {
        panic!("expected mention-aware submit");
    };
    assert_eq!(prompt, "review @same.txt");
    assert_eq!(bindings.bindings().len(), 1);
    assert_eq!(
        bindings.bindings()[0].target,
        orca_runtime::mentions::MentionTarget::File {
            root: PathBuf::from("/workspace/backend"),
            path: "same.txt".to_string(),
            kind: orca_runtime::mentions::MentionFileKind::File,
        }
    );
}

#[test]
fn idle_submit_with_open_empty_mention_popup_keeps_unbound_at_literal() {
    let (mut state, _rx) = test_state();
    let mut config = test_config(HistoryMode::Record);
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let (action_tx, action_rx) = mpsc::unbounded();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim_state = VimState::new(false);
    let prompt = "@oai/sky还能逆向吗";
    let mut textarea = make_textarea_with_text(prompt, &vim_state, &theme);
    state.mention.phase = Some(orca_file_search::SearchPhase::Scanning);
    assert!(state.mention.candidates.is_empty());
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

    crate::idle_key_actions::handle_idle_key(
        &Event::Key(key),
        &key,
        &mut state,
        &mut config,
        &shared_config,
        &action_tx,
        &mut textarea,
        &mut vim_state,
        &theme,
    );

    let action = action_rx.try_recv().expect("literal submit action");
    let UserAction::SubmitWithMentions {
        prompt, bindings, ..
    } = action
    else {
        panic!("expected mention-aware submit boundary");
    };
    assert_eq!(prompt, "@oai/sky还能逆向吗");
    assert!(bindings.is_empty());
}
