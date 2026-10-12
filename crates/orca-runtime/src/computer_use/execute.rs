use std::path::Path;
use std::time::Duration;

use orca_computer_use::call::ComputerCall;
use orca_computer_use::client::CallError;
use orca_computer_use::observation::render_observation;
use orca_computer_use::protocol::{
    ActParams, Action, AppInfo, ErrorKind, Observation, ObserveParams, Reply, Request, Target,
};
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::ImageInput;
use orca_core::model::{FLASH_MODEL, accepts_tool_images};
use orca_core::tool_images::tool_image;
use orca_core::tool_types::{ToolRequest, ToolResult};

use super::{ComputerUseService, service, turn::begin_turn};
use crate::extension::RuntimeExtensionStores;

pub(crate) fn execute(
    request: &ToolRequest,
    config: &RunConfig,
    subagent_depth: u32,
    stores: RuntimeExtensionStores<'_>,
    cwd: &Path,
    cancel: &CancelToken,
) -> ToolResult {
    let timeout = if request
        .raw_arguments
        .as_deref()
        .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
        .is_some_and(|args| args["action"] == "launch_app")
    {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(30)
    };
    execute_with_timeout(
        request,
        config,
        subagent_depth,
        stores,
        cwd,
        cancel,
        timeout,
    )
}

fn execute_with_timeout(
    request: &ToolRequest,
    config: &RunConfig,
    subagent_depth: u32,
    stores: RuntimeExtensionStores<'_>,
    cwd: &Path,
    cancel: &CancelToken,
    timeout: Duration,
) -> ToolResult {
    let existing_service = stores.thread_store().get::<ComputerUseService>();
    if let Err(error) =
        super::prepare::ensure_visible(config, subagent_depth, existing_service.as_deref())
    {
        return ToolResult::failed_before_start(request, error, None);
    }
    let call = match serde_json::from_str(request.raw_arguments.as_deref().unwrap_or("{}"))
        .map_err(|error| error.to_string())
        .and_then(|args| ComputerCall::parse(&args))
    {
        Ok(call) => call,
        Err(error) => return ToolResult::invalid_input(request, error),
    };
    let target = request.target.as_deref().unwrap_or_default();
    let secure_approved = target.ends_with("#secure");
    let app = target.strip_suffix("#secure").unwrap_or(target);
    if !matches!(call, ComputerCall::ListApps) && app.is_empty() {
        return ToolResult::failed(request, "computer requires an approved app target", None);
    }
    if cancel.is_cancelled() {
        return ToolResult::cancelled_before_start(request, "the turn was cancelled");
    }
    let service = match service(stores.thread_store(), cwd, config) {
        Ok(service) => service,
        Err(error) => return ToolResult::failed_before_start(request, error, None),
    };
    if let Err(error) = super::prepare::check_permissions(&service) {
        return ToolResult::failed_before_start(request, error, None);
    }
    let model = config.model.as_deref().unwrap_or(FLASH_MODEL);
    let include_screenshot = config.model.supports_images(model) && accepts_tool_images(model);
    let driver_request = match &call {
        ComputerCall::ListApps => Request::ListApps,
        ComputerCall::LaunchApp { open, .. } => Request::LaunchApp {
            app: app.into(),
            open: open.clone(),
        },
        ComputerCall::Observe {
            window,
            query,
            max_elements,
            ..
        } => Request::Observe(ObserveParams {
            app: app.into(),
            window: *window,
            query: query.clone(),
            max_elements: *max_elements,
            include_screenshot,
        }),
        ComputerCall::Act {
            window,
            action,
            observe_after,
            ..
        } => Request::Act(ActParams {
            app: app.into(),
            window: window.or_else(|| service.latest_window(app)),
            action: action.clone(),
            foreground_fallback: config.computer_use.foreground_fallback,
            observe_after: *observe_after,
            include_screenshot,
            secure_approved,
        }),
    };
    begin_turn(&service, stores.turn_store(), cancel);
    if cancel.is_cancelled() {
        return ToolResult::cancelled_before_start(request, "the turn was cancelled");
    }
    let reply = match service.call(driver_request, timeout) {
        Ok(reply) => reply,
        Err(error) if cancel.is_cancelled() => {
            service.reset();
            let mut message = "computer use was stopped by the user (Esc)".to_string();
            if let CallError::Driver(error) = error {
                message.push_str("; ");
                message.push_str(&error.message);
            }
            return ToolResult::cancelled(request, message, None);
        }
        Err(error @ (CallError::TimedOut | CallError::Disconnected)) => {
            service.reset();
            let stopped = if matches!(error, CallError::Disconnected) {
                "stopped"
            } else {
                "was restarted"
            };
            // Disconnection and timeout have the same recovery instruction.
            return ToolResult::failed(
                request,
                format!(
                    "the computer-use driver did not answer in {} s and {stopped}; observe again before acting",
                    timeout.as_secs()
                ),
                None,
            );
        }
        Err(CallError::Driver(error)) => {
            if error.kind == ErrorKind::NeedsSecureApproval {
                return ToolResult::failed(
                    request,
                    "The driver detected text entry into a password field that was not separately approved. Observe the app again, then target the secure field by element number so Orca can ask for separate approval; no text was entered.",
                    None,
                );
            }
            let mut message = error.message;
            if matches!(error.kind, ErrorKind::AppAmbiguous | ErrorKind::AppNotFound) {
                for app in &error.candidates {
                    message.push('\n');
                    message.push_str(&app_line(app));
                    append_windows(&mut message, app);
                }
            }
            return ToolResult::failed(request, message, None);
        }
    };
    let mut images = Vec::new();
    let output = match reply {
        Reply::Apps { apps } => apps
            .iter()
            .map(|app| {
                let mut text = app_line(app);
                append_windows(&mut text, app);
                text
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::Launched { app: launched } => {
            if !matches!(call, ComputerCall::LaunchApp { .. }) || launched.bundle_id != app {
                return ToolResult::failed(
                    request,
                    format!(
                        "The launched app's bundle ID {:?} does not match the approved ID {app:?}",
                        launched.bundle_id
                    ),
                    None,
                );
            }
            let app = launched;
            let mut text = format!("launched {} ({})", app.name, app.bundle_id);
            append_windows(&mut text, &app);
            text
        }
        Reply::Observed { observation } => observation_text(&service, observation, &mut images),
        Reply::Acted { outcome } => {
            let ComputerCall::Act { action, .. } = &call else {
                return ToolResult::failed(
                    request,
                    "computer-use driver returned an unexpected action result",
                    None,
                );
            };
            let mut text = format!("{} {}: done", call.action_name(), action_target(action));
            if outcome.foreground_used {
                text.push_str("; used the foreground");
            }
            if let Some(note) = outcome.note {
                text.push_str("; ");
                text.push_str(&note);
            }
            if let Some(position) = outcome.scroll_position {
                text.push_str(&format!("; scrolled to {:.0}%", position * 100.0));
            }
            if let Some(observation) = outcome.observation {
                text.push_str("\n\n");
                text.push_str(&observation_text(&service, observation, &mut images));
            }
            text
        }
        _ => {
            return ToolResult::failed(
                request,
                "computer-use driver returned an unexpected result",
                None,
            );
        }
    };
    ToolResult::completed(request, output, false).with_images(images)
}

fn app_line(app: &AppInfo) -> String {
    format!(
        "{} — {} (pid {})",
        app.name,
        app.bundle_id,
        app.pid
            .map_or_else(|| "not running".into(), |pid| pid.to_string())
    )
}

fn append_windows(text: &mut String, app: &AppInfo) {
    for window in &app.windows {
        text.push_str(&format!("\n  window {}: \"{}\"", window.id, window.title));
    }
}

fn observation_text(
    service: &ComputerUseService,
    observation: Observation,
    images: &mut Vec<ImageInput>,
) -> String {
    service.record_observation(&observation);
    let mut text = render_observation(&observation);
    if let Some(screenshot) = observation.screenshot {
        match tool_image("image/png", screenshot.png_base64) {
            Ok(image) => {
                images.push(image);
                text.push_str("\n[1 image attached]");
            }
            Err(rejected) => {
                text.push('\n');
                text.push_str(&rejected.note());
            }
        }
    }
    text
}

fn action_target(action: &Action) -> String {
    let target = match action {
        Action::Click { target, .. } | Action::Scroll { target, .. } => Some(*target),
        Action::Type { target, .. } | Action::Key { target, .. } => *target,
    };
    match target {
        Some(Target::Element { element }) => format!("[{element}]"),
        Some(Target::Point { point: [x, y] }) => format!("({x},{y})"),
        None => "focused element".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer_use::ComputerUseService;
    use crate::extension::ExtensionData;
    use orca_computer_use::fake::{FakeDesktop, FakeLauncher};
    use orca_core::approval_types::ActionKind;
    use orca_core::model::ModelSelection;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    fn insert_service(store: &ExtensionData, launcher: &FakeLauncher) {
        store.insert(ComputerUseService::new_for_test(Arc::new(launcher.clone())));
    }

    fn flash_config() -> RunConfig {
        let mut config = RunConfig::default();
        config.computer_use.interactive = true;
        config.model = ModelSelection::from_unchecked(Some("deepseek-flash".into()));
        config
    }

    fn request(args: Value, target: Option<&str>) -> ToolRequest {
        ToolRequest {
            id: "computer-1".into(),
            name: ToolName::Computer,
            action: ActionKind::Computer,
            target: target.map(str::to_owned),
            raw_arguments: Some(args.to_string()),
        }
    }

    fn observe_request(app: &str) -> ToolRequest {
        request(json!({"action":"observe", "app":"TextEdit"}), Some(app))
    }

    fn wait_until(check: impl Fn() -> bool) {
        let started = Instant::now();
        while !check() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "condition did not become true within 2 s"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn part5_launch_binding_rejects_a_reply_for_a_different_approved_id() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.launch_reply_override = Some(orca_computer_use::protocol::AppInfo {
            bundle_id: "com.apple.textedit".into(),
            name: "Impostor".into(),
            pid: Some(99),
            category: None,
            windows: vec![],
        });
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let request = request(
            json!({"action":"launch_app", "app":"TextEdit"}),
            Some("com.apple.TextEdit"),
        );
        let result = execute(
            &request,
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Failed);
        let error = result.error.as_deref().unwrap();
        assert!(error.contains("com.apple.TextEdit"));
        assert!(error.contains("com.apple.textedit"));
        launcher.desktop(|desktop| desktop.launch_reply_override = None);
        let result = execute(
            &request,
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
    }

    #[test]
    fn observe_returns_the_header_the_tree_and_one_png() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .starts_with("TextEdit \"probe.rtf\" [window 1]: screenshot"),
            "{:?}",
            result.output
        );
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .contains("[1] button \"Bold\"")
        );
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .ends_with("[1 image attached]")
        );
        assert_eq!(result.images.len(), 1);
        assert!(thread_store.get::<ComputerUseService>().unwrap().is_secure(
            "com.apple.TextEdit",
            Some(1),
            14
        ));
    }

    #[test]
    fn a_model_that_cannot_see_images_gets_no_screenshot_request() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let mut config = flash_config();
        config.model = ModelSelection::from_unchecked(Some("deepseek-v4-pro".into()));
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &config,
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(!launcher.desktop(|d| d.last_observe.as_ref().unwrap().include_screenshot));
        assert!(result.images.is_empty());
    }

    #[test]
    fn the_foreground_note_reaches_the_result() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.background_fails = true;
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let result = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .contains("used the foreground"),
            "{:?}",
            result.output
        );
    }

    #[test]
    fn a_timed_out_driver_is_restarted_with_a_note() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let cancel = CancelToken::new();
        thread_store
            .get::<ComputerUseService>()
            .unwrap()
            .hello()
            .unwrap();
        launcher.desktop(|d| d.answer_after = Some(Duration::from_millis(100)));
        let result = execute_with_timeout(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &cancel,
            Duration::from_millis(50),
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result.error.as_deref().unwrap().contains("was restarted"),
            "{:?}",
            result.error
        );
        launcher.desktop(|d| d.answer_after = None);
        // Authorization calls hello before the next execution after a restart.
        thread_store
            .get::<ComputerUseService>()
            .unwrap()
            .hello()
            .unwrap();
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(launcher.launches(), 2);
        assert!(
            launcher.desktop(|d| d.turn_active),
            "the replacement driver must listen for Esc in this turn"
        );
    }

    #[test]
    fn esc_cancels_the_turn_and_the_listener_ends_with_it() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let cancel = CancelToken::new();
        let _ = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &cancel,
        );
        assert!(launcher.desktop(|d| d.turn_active));
        launcher.emit_esc();
        wait_until(|| cancel.is_cancelled());
        drop(turn_store);
        wait_until(|| !launcher.desktop(|d| d.turn_active));
        // Ending the guard clears the recorded token; a new turn gets its own token.
        let next_turn = ExtensionData::new("next-turn");
        let next_cancel = CancelToken::new();
        let next_request = observe_request("com.apple.TextEdit");
        let mut invocation = crate::tool_invocation::ToolInvocation {
            requested: next_request.clone(),
            effective: next_request,
            action: Some(ActionKind::Computer),
            approval_preview: None,
        };
        crate::computer_use::prepare::prepare_invocation(
            &mut invocation,
            &flash_config(),
            &thread_store.get::<ComputerUseService>().unwrap(),
        )
        .unwrap();
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &next_turn),
            Path::new("/"),
            &next_cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        launcher.emit_esc();
        wait_until(|| next_cancel.is_cancelled());
    }
    #[test]
    fn esc_interrupts_an_observation_in_flight() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = Arc::new(ExtensionData::new("thread"));
        let turn_store = Arc::new(ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let cancel = CancelToken::new();
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        assert_eq!(
            execute(
                &observe_request("com.apple.TextEdit"),
                &flash_config(),
                0,
                stores,
                Path::new("/"),
                &cancel
            )
            .status,
            ToolStatus::Completed
        );
        let (started, received) = std::sync::mpsc::channel();
        launcher.desktop(|d| {
            d.request_started = Some(started);
            d.answer_after = Some(Duration::from_millis(800));
        });
        let (finished, result) = std::sync::mpsc::channel();
        let calling_cancel = cancel.clone();
        let call = thread::spawn(move || {
            let output = execute(
                &observe_request("com.apple.TextEdit"),
                &flash_config(),
                0,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
                Path::new("/"),
                &calling_cancel,
            );
            finished.send(output).unwrap();
        });
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            orca_computer_use::protocol::Request::Observe(_)
        ));
        launcher.emit_esc();
        let output = result
            .recv_timeout(Duration::from_millis(500))
            .expect("Esc must release the in-flight call without waiting for its reply");
        assert!(cancel.is_cancelled());
        assert_eq!(output.status, ToolStatus::Cancelled);
        assert_eq!(
            output.error.as_deref(),
            Some("computer use was stopped by the user (Esc)")
        );
        call.join().unwrap();
    }

    #[test]
    fn actions_use_the_approved_bundle_id_and_secure_suffix() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit_and_calculator());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let result = execute(
            &request(
                json!({"action":"type", "app":"Calculator", "text":"secret", "target":{"element":14}}),
                Some("com.apple.TextEdit#secure"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(launcher.desktop(|d| d.actions_on("com.apple.TextEdit")), 1);
        assert_eq!(
            launcher.desktop(|d| d.actions_on("com.apple.calculator")),
            0
        );
    }

    #[test]
    fn auto_receives_a_screenshot_and_a_normal_turn_removes_the_listener() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = Arc::new(ExtensionData::new("thread"));
        insert_service(&thread_store, &launcher);
        let cancel = CancelToken::new();
        let registry = crate::tasks::TaskRegistry::new("test".into());
        let mut costs = crate::cost::CostTracker::new(None);
        let turn = crate::lifecycle::RuntimeTurnState::new_with_thread_extensions(
            &mut costs,
            &cancel,
            &registry,
            thread_store.clone(),
            "test:turn",
        )
        .into_loop_state();
        let mut config = flash_config();
        config.model = ModelSelection::from_unchecked(Some("auto".into()));
        let stores = turn.runtime.extensions.extension_context().stores();
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &config,
            0,
            stores,
            Path::new("/"),
            &cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.images.len(), 1);
        assert!(launcher.desktop(|d| d.turn_active));
        drop(turn);
        wait_until(|| !launcher.desktop(|d| d.turn_active));
        launcher.emit_esc();
        // Flush the event reader through a subsequent reply before checking the old token.
        thread_store
            .get::<ComputerUseService>()
            .unwrap()
            .call(
                orca_computer_use::protocol::Request::ListApps,
                Duration::from_secs(2),
            )
            .unwrap();
        assert!(!cancel.is_cancelled());
    }
    #[test]
    fn list_launch_scroll_and_foreground_errors_are_formatted() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let cancel = CancelToken::new();
        let mut config = flash_config();
        for (args, target, expected) in [
            (
                json!({"action":"list_apps"}),
                None,
                "TextEdit — com.apple.TextEdit (pid 42)\n  window 1: \"probe.rtf\"",
            ),
            (
                json!({"action":"launch_app", "app":"TextEdit", "open":["probe.rtf"]}),
                Some("com.apple.TextEdit"),
                "launched TextEdit (com.apple.TextEdit)\n  window 1: \"probe.rtf\"",
            ),
            (
                json!({"action":"scroll", "app":"TextEdit", "target":{"point":[312,190]}, "direction":"down", "amount":3, "observe":false}),
                Some("com.apple.TextEdit"),
                "scroll (312,190): done; scrolled to 30%",
            ),
            (
                json!({"action":"key", "app":"TextEdit", "keys":["cmd", "s"], "observe":false}),
                Some("com.apple.TextEdit"),
                "key focused element: done",
            ),
        ] {
            let result = execute(
                &request(args, target),
                &config,
                0,
                stores,
                Path::new("/"),
                &cancel,
            );
            assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
            assert_eq!(result.output.as_deref(), Some(expected));
            assert!(result.images.is_empty());
        }
        launcher.desktop(|d| d.background_fails = true);
        let click = request(
            json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
            Some("com.apple.TextEdit"),
        );
        config.computer_use.foreground_fallback = false;
        let result = execute(&click, &config, 0, stores, Path::new("/"), &cancel);
        assert_eq!(
            result.error.as_deref(),
            Some("Background delivery failed; foreground fallback is disabled")
        );
        config.computer_use.foreground_fallback = true;
        launcher.desktop(|d| d.user_active = true);
        let result = execute(&click, &config, 0, stores, Path::new("/"), &cancel);
        assert_eq!(
            result.error.as_deref(),
            Some("The user is active; foreground fallback was abandoned")
        );
        let secure = request(
            json!({"action":"type", "app":"TextEdit", "target":{"element":14}, "text":"secret"}),
            Some("com.apple.TextEdit"),
        );
        let result = execute(&secure, &config, 0, stores, Path::new("/"), &cancel);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "The driver detected text entry into a password field that was not separately approved. Observe the app again, then target the secure field by element number so Orca can ask for separate approval; no text was entered."
            )
        );
    }
    #[test]
    fn esc_during_turn_start_prevents_the_observation() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        insert_service(&thread_store, &launcher);
        thread_store
            .get::<ComputerUseService>()
            .unwrap()
            .hello()
            .unwrap();
        let (started, received) = std::sync::mpsc::channel();
        launcher.desktop(|d| {
            d.request_started = Some(started);
            d.answer_after = Some(Duration::from_millis(100));
        });
        let cancel = CancelToken::new();
        let executing_cancel = cancel.clone();
        let call = thread::spawn(move || {
            execute(
                &observe_request("com.apple.TextEdit"),
                &flash_config(),
                0,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
                Path::new("/"),
                &executing_cancel,
            )
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            orca_computer_use::protocol::Request::Turn { active: true }
        );
        launcher.emit_esc();
        let result = call.join().unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(result.status, ToolStatus::Cancelled, "{:?}", result.output);
        assert!(
            launcher.desktop(|d| d.last_observe.is_none()),
            "Esc at turn start must prevent the request"
        );
    }
    #[test]
    fn a_flash_model_with_images_disabled_gets_no_screenshot_request() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let mut config = flash_config();
        config.model = ModelSelection::parse_with_models(
            Some("deepseek-flash".into()),
            std::collections::BTreeMap::from([(
                "deepseek-flash".into(),
                orca_core::model::ModelDefinition {
                    supports_images: Some(false),
                },
            )]),
        )
        .unwrap();
        let result = execute(
            &observe_request("com.apple.TextEdit"),
            &config,
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(!launcher.desktop(|d| d.last_observe.as_ref().unwrap().include_screenshot));
        assert!(result.images.is_empty());
    }
    #[test]
    fn hidden_computer_execution_never_launches_the_driver() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let mut config = flash_config();
        config
            .permission_rules
            .rules
            .push(orca_core::approval_rules::PermissionRule::whole_tool(
                "computer",
                orca_core::approval_types::Decision::Deny,
            ));
        let result = execute(
            &request(json!({"action":"list_apps"}), None),
            &config,
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(launcher.launches(), 0);
    }

    #[test]
    fn an_explicit_action_window_wins_over_the_latest_observed_window() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.add_window(
            "com.apple.TextEdit",
            orca_computer_use::protocol::WindowInfo {
                id: 2,
                title: "B".into(),
            },
        );
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let cancel = CancelToken::new();
        let result = execute(
            &request(
                json!({"action":"observe", "app":"TextEdit", "window":2}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        let (started, received) = std::sync::mpsc::channel();
        launcher.desktop(|d| d.request_started = Some(started));
        let result = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "window":1, "target":{"element":1}, "observe":false}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        let Request::Act(params) = received.recv_timeout(Duration::from_secs(2)).unwrap() else {
            panic!("act")
        };
        assert_eq!(params.window, Some(1));
        // Omitting window still uses the last successfully observed window B.
        let result = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "target":{"element":1}, "observe":false}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &cancel,
        );
        assert_eq!(result.status, ToolStatus::Completed);
        let Request::Act(params) = received.recv_timeout(Duration::from_secs(2)).unwrap() else {
            panic!("act")
        };
        assert_eq!(params.window, Some(2));
    }

    #[test]
    fn successful_action_without_an_observation_keeps_its_note() {
        struct ClosingDriver(FakeDesktop);
        impl orca_computer_use::serve::DriverHandler for ClosingDriver {
            fn handle(
                &mut self,
                request: Request,
                events: &orca_computer_use::serve::LineWriter,
            ) -> Result<Reply, orca_computer_use::protocol::DriverError> {
                if matches!(request, Request::Act(_)) {
                    return Ok(Reply::Acted {
                        outcome: orca_computer_use::protocol::ActOutcome {
                            foreground_used: false,
                            note: Some("The window closed after the action; observe again.".into()),
                            scroll_position: None,
                            observation: None,
                        },
                    });
                }
                self.0.handle(request, events)
            }
            fn shutdown(&mut self) {
                self.0.shutdown();
            }
        }
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        thread_store.insert(ComputerUseService::new_for_test(Arc::new(
            orca_computer_use::inprocess::InProcessLauncher::new(ClosingDriver(
                FakeDesktop::with_textedit(),
            )),
        )));
        let result = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .contains("The window closed after the action; observe again."),
            "{:?}",
            result.output
        );
        assert!(result.images.is_empty());
    }

    #[test]
    fn a_point_target_needing_secure_approval_gives_a_safe_retry_instruction() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let result = execute(
            &request(
                json!({"action":"key", "app":"TextEdit", "keys":["p"], "target":{"point":[30,160]}}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("Observe the app again"),
            "{:?}",
            result.error
        );
        assert_eq!(launcher.desktop(|d| d.actions_on("com.apple.TextEdit")), 0);
    }

    #[test]
    fn runtime_formats_truncated_and_title_bar_only_observations() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new_for_test(Arc::new(launcher));
        let Reply::Observed { mut observation } = service
            .call(
                Request::Observe(ObserveParams {
                    app: "com.apple.TextEdit".into(),
                    window: None,
                    query: None,
                    max_elements: 300,
                    include_screenshot: false,
                }),
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("observation")
        };
        observation.truncated = true;
        observation.no_element_tree = true;
        let mut images = vec![];
        let text = observation_text(&service, observation, &mut images);
        assert!(text.contains("[window 1]"));
        assert!(text.contains("cut off after"));
        assert!(text.contains("time limit"));
        assert!(text.contains("no element tree; use point targets"));
    }
    #[test]
    fn a_driver_error_after_turn_cancellation_is_reported_as_cancelled() {
        struct CancellingDriver {
            desktop: FakeDesktop,
            cancel: CancelToken,
        }
        impl orca_computer_use::serve::DriverHandler for CancellingDriver {
            fn handle(
                &mut self,
                request: Request,
                events: &orca_computer_use::serve::LineWriter,
            ) -> Result<Reply, orca_computer_use::protocol::DriverError> {
                if matches!(request, Request::Act(_)) {
                    self.cancel.cancel();
                    return Err(orca_computer_use::protocol::DriverError {
                        kind: ErrorKind::Failed,
                        message: "The driver stopped the action; could not restore the pointer"
                            .into(),
                        candidates: vec![],
                    });
                }
                self.desktop.handle(request, events)
            }
            fn shutdown(&mut self) {
                self.desktop.shutdown();
            }
        }
        let cancel = CancelToken::new();
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        thread_store.insert(ComputerUseService::new_for_test(Arc::new(
            orca_computer_use::inprocess::InProcessLauncher::new(CancellingDriver {
                desktop: FakeDesktop::with_textedit(),
                cancel: cancel.clone(),
            }),
        )));
        let result = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &cancel,
        );
        assert!(cancel.is_cancelled());
        assert_eq!(result.status, ToolStatus::Cancelled, "{:?}", result.error);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "computer use was stopped by the user (Esc); The driver stopped the action; could not restore the pointer"
            )
        );
        assert_eq!(
            result.terminal().started,
            orca_core::tool_types::ToolInvocationStarted::Yes
        );
    }
}
