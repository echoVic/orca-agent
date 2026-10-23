use super::*;
use crate::protocol::TuiEvent;
use crate::types::{AppState, ApprovalOption};
use orca_computer_use::fake::FakeDesktop;
use orca_runtime::runtime_host::test_support::FakeComputerExecutor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct ComputerTui {
    runtime: TuiAgentRuntime,
    state: AppState,
    events: Receiver<TuiEvent>,
    actions: Sender<UserAction>,
    launcher: orca_computer_use::fake::FakeLauncher,
    _workspace: tempfile::TempDir,
}

impl ComputerTui {
    fn start(desktop: FakeDesktop) -> Self {
        let executor = FakeComputerExecutor::new(desktop);
        let launcher = executor.launcher.clone();
        let host = RuntimeHost::start_with_executor(Arc::new(executor)).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(workspace.path().to_path_buf());
        config.history_mode = orca_core::config::HistoryMode::Record;
        config.approval_mode = orca_core::approval_types::ApprovalMode::AutoEdit;
        config.computer_use.interactive = true;
        let config = Arc::new(Mutex::new(config));
        let (event_tx, events) = crossbeam_channel::unbounded();
        let (actions, action_rx) = crossbeam_channel::unbounded();
        let runtime = TuiAgentRuntime::spawn_with_dispatch_capacities(
            action_rx,
            event_tx.clone(),
            128,
            128,
            TuiSurfaceTaskControl::new(),
            host,
            move |control, commands, host| {
                crate::hosted_controller::hosted_tui_controller_loop(
                    config,
                    Arc::new(Mutex::new(None)),
                    event_tx,
                    commands,
                    control,
                    crate::bridge::PendingWorkflowNotifications::new(),
                    host,
                )
            },
        )
        .unwrap();
        let state = AppState::new(actions.clone(), "test".into(), "mock".into(), "/tmp".into());
        Self {
            runtime,
            state,
            events,
            actions,
            launcher,
            _workspace: workspace,
        }
    }

    fn send(&mut self, arguments: serde_json::Value) {
        self.actions
            .send(UserAction::Submit(format!("computer {arguments}")))
            .unwrap();
    }

    fn until(&mut self, done: impl Fn(&TuiEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("computer TUI event");
            let Ok(Some(event)) =
                crate::attachment_routing::accept_attached_tui_event(&mut self.state, event)
            else {
                continue;
            };
            let finished = done(&event);
            let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
            let mut vim = crate::vim::VimState::new(false);
            let mut textarea = crate::composer_textarea::make_textarea(&vim, &theme);
            let mut presentation = crate::terminal_presentation::TerminalPresentation::new(
                false,
                crate::terminal_presentation::TerminalPresentationProfile {
                    osc9_supported: false,
                    tmux_passthrough: false,
                },
            );
            crate::runtime_event_actions::handle_runtime_event(
                event,
                &mut self.state,
                &self.actions,
                &crate::bridge::PendingWorkflowNotifications::new(),
                &mut textarea,
                &mut vim,
                &theme,
                &mut presentation,
            );
            if finished {
                return;
            }
        }
    }

    fn approval(&mut self) {
        self.until(|event| matches!(event, TuiEvent::ApprovalNeeded { .. }));
        assert!(self.state.approval_dialog.is_some());
    }

    fn answer(&mut self, key: char) {
        crate::approval_dialog_actions::handle_approval_dialog_key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(key),
                crossterm::event::KeyModifiers::NONE,
            ),
            &mut self.state,
            &self.actions,
        );
    }

    fn completed(&mut self) {
        self.until(|event| matches!(event, TuiEvent::SessionCompleted { .. }));
        assert!(self.state.approval_dialog.is_none());
    }
}

impl Drop for ComputerTui {
    fn drop(&mut self) {
        self.runtime.shutdown().unwrap();
    }
}

#[test]
fn computer_surface_dialog_session_grant_covers_same_app_background_and_foreground_actions() {
    let _home = crate::test_support::isolate_orca_home();
    for foreground in [false, true] {
        let mut desktop = FakeDesktop::with_textedit_and_calculator();
        desktop.background_fails = foreground;
        let mut tui = ComputerTui::start(desktop);
        tui.send(serde_json::json!({"action":"observe","app":"TextEdit"}));
        tui.approval();
        let dialog = tui.state.approval_dialog.as_ref().unwrap();
        assert_eq!(dialog.target.as_deref(), Some("com.apple.TextEdit"));
        assert_eq!(
            dialog.options,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTool,
                ApprovalOption::AlwaysTargetSaved,
                ApprovalOption::AlwaysToolSaved,
                ApprovalOption::Deny
            ]
        );
        let screen = crate::test_support::frame_string(&mut tui.state, 140, 40);
        for label in [
            "Allow this app this session",
            "Allow all apps this session",
            "Always allow this app (saved)",
            "Always allow all apps (saved)",
        ] {
            assert!(screen.contains(label), "{screen}");
        }
        tui.answer('2');
        tui.completed();
        for arguments in [
            serde_json::json!({"action":"click","app":"com.apple.TextEdit","target":{"element":1}}),
            serde_json::json!({"action":"type","app":"TextEdit","target":{"element":2},"text":"hello"}),
        ] {
            tui.send(arguments);
            tui.until(|event| {
                matches!(event, TuiEvent::SessionCompleted { .. })
                    || matches!(event, TuiEvent::ApprovalNeeded { .. })
            });
            assert!(
                tui.state.approval_dialog.is_none(),
                "session grant must skip the dialog"
            );
            if tui.state.status != crate::types::AppStatus::Idle {
                tui.completed();
            }
        }
        tui.launcher
            .desktop(|desktop| assert_eq!(desktop.actions_on("com.apple.TextEdit"), 2));
        tui.send(serde_json::json!({"action":"observe","app":"Calculator"}));
        tui.approval();
        assert_eq!(
            tui.state
                .approval_dialog
                .as_ref()
                .unwrap()
                .target
                .as_deref(),
            Some("com.apple.calculator")
        );
        tui.answer('1');
        tui.completed();
        tui.send(serde_json::json!({"action":"type","app":"TextEdit","target":{"element":14},"text":"secret"}));
        tui.approval();
        let dialog = tui.state.approval_dialog.as_ref().unwrap();
        assert_eq!(dialog.target.as_deref(), Some("com.apple.TextEdit#secure"));
        assert_eq!(
            dialog.options,
            vec![ApprovalOption::Once, ApprovalOption::Deny]
        );
        let screen = crate::test_support::frame_string(&mut tui.state, 140, 40);
        assert!(screen.contains("Allow typing this once"), "{screen}");
        tui.answer('4');
        tui.completed();
    }
}

#[test]
fn computer_surface_terminal_and_ide_dialogs_keep_exact_app_grants() {
    let _home = crate::test_support::isolate_orca_home();
    for (bundle, name) in [
        ("com.apple.Terminal", "Terminal"),
        ("com.microsoft.VSCode", "Code"),
    ] {
        let mut tui = ComputerTui::start(FakeDesktop::with_app(bundle, name));
        tui.send(serde_json::json!({"action":"observe","app":name}));
        tui.approval();
        let dialog = tui.state.approval_dialog.as_ref().unwrap();
        assert_eq!(dialog.target.as_deref(), Some(bundle));
        assert_eq!(
            dialog.options,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTargetSaved,
                ApprovalOption::Deny
            ]
        );
        tui.answer('4');
        tui.completed();
    }
}
