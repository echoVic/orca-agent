use std::path::Path;
use std::time::Duration;

use orca_computer_use::call::ComputerCall;
use orca_computer_use::client::CallError;
use orca_computer_use::observation::render_observation;
use orca_computer_use::protocol::{
    ActParams, Action, AppInfo, ErrorKind, Observation, ObserveParams, Reply, Request, Target,
};
use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::computer_apps::ComputerTarget;
use orca_core::config::RunConfig;
use orca_core::conversation::ImageInput;
use orca_core::model::{FLASH_MODEL, accepts_tool_images};
use orca_core::tool_images::tool_image;
use orca_core::tool_types::{ToolRequest, ToolResult, ToolStatus};

use super::budget::FailureBudget;
use super::ownership::TurnWindows;
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
    let app = ComputerTarget::parse(request.target.as_deref().unwrap_or_default()).app;
    let app = app.as_str();
    // list_apps has no app; every other call counts toward its app's budget.
    let budget = (!app.is_empty()).then(|| FailureBudget::for_turn(stores.turn_store()));
    if let Some(stop) = budget.as_ref().and_then(|budget| budget.stopped(app)) {
        return ToolResult::failed_before_start(request, stop, None);
    }
    let mut result = run_call(
        request,
        config,
        subagent_depth,
        stores,
        cwd,
        cancel,
        timeout,
    );
    let failed = match result.status {
        ToolStatus::Failed => Some(true),
        ToolStatus::Completed => Some(false),
        // A cancellation is the user's, not the app's failure.
        _ => None,
    };
    if let (Some(budget), Some(failed)) = (budget, failed)
        && let Some(note) = budget.record(app, failed)
    {
        result.append_error_note(&note);
    }
    result
}

fn run_call(
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
    let target = ComputerTarget::parse(request.target.as_deref().unwrap_or_default());
    let secure_approved = target.secure;
    let app = target.app.as_str();
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
    let turn_windows = TurnWindows::for_turn(stores.turn_store());
    // The user's windows this call must not use: the app's, but the one
    // approved for this call. Full-auto asks about no window.
    let user_windows: Vec<u32> = if config.approval_mode == ApprovalMode::FullAuto {
        vec![]
    } else {
        turn_windows
            .user_windows(&service, app)
            .into_iter()
            .map(|window| window.id)
            .filter(|&id| Some(id) != target.window)
            .collect()
    };
    // An act without a window inherits the latest observed one.
    let inherited = match &call {
        ComputerCall::Act { window: None, .. } => service.latest_window(app),
        _ => None,
    };
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
            user_windows,
        }),
        ComputerCall::Act {
            window,
            action,
            observe_after,
            ..
        } => Request::Act(ActParams {
            app: app.into(),
            window: window.or(inherited),
            action: action.clone(),
            foreground_fallback: config.computer_use.foreground_fallback,
            observe_after: *observe_after,
            include_screenshot,
            secure_approved,
            user_windows,
        }),
    };
    begin_turn(&service, stores.turn_store(), cancel);
    if cancel.is_cancelled() {
        return ToolResult::cancelled_before_start(request, "the turn was cancelled");
    }
    let retry = inherited.map(|_| match &driver_request {
        Request::Act(params) => Request::Act(ActParams {
            window: None,
            ..params.clone()
        }),
        other => other.clone(),
    });
    // A call that reaches the driver was approved (or runs in full-auto): its
    // app's window titles may be listed from now on.
    if !matches!(call, ComputerCall::ListApps) {
        service.note_used_app(app);
    }
    let mut reply = service.call(driver_request, timeout);
    // The inherited window is gone (the app's last window closed): forget it
    // and let the driver choose, which also reaches app-level shortcuts.
    if let (Some(retry), Err(CallError::Driver(error))) = (retry, &reply)
        && error.kind == ErrorKind::WindowNotFound
    {
        service.forget_window(app);
        reply = service.call(retry, timeout);
    }
    // An app's first result this session carries its risk warning, whatever
    // the outcome, so calls approved without a panel (full-auto, rules,
    // session grants) show it too.
    let warning = service.take_risk_warning(app);
    let noted = |text: String| match &warning {
        Some(warning) => format!("{warning}\n{text}"),
        None => text,
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) if cancel.is_cancelled() => {
            service.reset();
            let mut message = "computer use was stopped by the user (Esc)".to_string();
            if let CallError::Driver(error) = error {
                message.push_str("; ");
                message.push_str(&error.message);
            }
            return ToolResult::cancelled(request, noted(message), None);
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
                noted(format!(
                    "the computer-use driver did not answer in {} s and {stopped}; observe again before acting",
                    timeout.as_secs()
                )),
                None,
            );
        }
        Err(CallError::Driver(error)) => {
            if error.kind == ErrorKind::NeedsSecureApproval {
                return ToolResult::failed(
                    request,
                    noted("The driver detected text entry into a password field that was not separately approved. Observe the app again, then target the secure field by element number so Orca can ask for separate approval; no text was entered.".into()),
                    None,
                );
            }
            let mut message = error.message;
            if matches!(error.kind, ErrorKind::AppAmbiguous | ErrorKind::AppNotFound) {
                for app in &error.candidates {
                    message.push('\n');
                    message.push_str(&app_line(app));
                    append_windows(&mut message, app, &service, &turn_windows);
                }
            }
            return ToolResult::failed(request, noted(message), None);
        }
    };
    let mut images = Vec::new();
    let output = match reply {
        Reply::Apps { apps } => apps
            .iter()
            .map(|app| {
                let mut text = app_line(app);
                append_windows(&mut text, app, &service, &turn_windows);
                text
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::Launched { app: launched } => {
            if !matches!(call, ComputerCall::LaunchApp { .. }) || launched.bundle_id != app {
                return ToolResult::failed(
                    request,
                    noted(format!(
                        "The launched app's bundle ID {:?} does not match the approved ID {app:?}",
                        launched.bundle_id
                    )),
                    None,
                );
            }
            let app = launched;
            let mut text = format!("launched {} ({})", app.name, app.bundle_id);
            if app.windows.is_empty() {
                text.push_str("; no window yet. ");
                text.push_str(orca_computer_use::messages::NO_WINDOW_NEXT_STEP);
            }
            append_windows(&mut text, &app, &service, &turn_windows);
            text
        }
        Reply::Observed { observation } => observation_text(&service, observation, &mut images),
        Reply::Acted { outcome } => {
            let ComputerCall::Act { action, .. } = &call else {
                return ToolResult::failed(
                    request,
                    noted("computer-use driver returned an unexpected action result".into()),
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
                noted("computer-use driver returned an unexpected result".into()),
                None,
            );
        }
    };
    ToolResult::completed(request, noted(output), false).with_images(images)
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

/// Appends an app's windows: with titles once the app has been used this
/// session, the user's marked, else only how many it has.
fn append_windows(
    text: &mut String,
    app: &AppInfo,
    service: &ComputerUseService,
    turn_windows: &TurnWindows,
) {
    if !service.used_app(&app.bundle_id) {
        let count = app.windows.len();
        text.push_str(&format!(
            " · {count} window{}",
            if count == 1 { "" } else { "s" }
        ));
        return;
    }
    let theirs: Vec<u32> = turn_windows
        .user_windows(service, &app.bundle_id)
        .into_iter()
        .map(|window| window.id)
        .collect();
    for window in &app.windows {
        text.push_str(&format!("\n  window {}: \"{}\"", window.id, window.title));
        if theirs.contains(&window.id) {
            text.push_str(" (yours)");
        }
        if window.minimized {
            text.push_str(" (minimized)");
        }
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
    fn a_warned_apps_first_result_this_session_carries_its_warning() {
        let launcher = FakeLauncher::new(FakeDesktop::with_app("com.apple.finder", "Finder"));
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let service = thread_store.get::<ComputerUseService>().unwrap();
        let mut outputs = Vec::new();
        for _ in 0..2 {
            let call = request(json!({"action":"observe", "app":"Finder"}), None);
            let mut invocation = crate::tool_invocation::ToolInvocation {
                requested: call.clone(),
                effective: call,
                action: Some(ActionKind::Computer),
                approval_preview: None,
            };
            crate::computer_use::prepare::prepare_invocation(
                &mut invocation,
                &flash_config(),
                &service,
            )
            .unwrap();
            let result = execute(
                &invocation.effective,
                &flash_config(),
                0,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
                Path::new("/"),
                &CancelToken::new(),
            );
            assert_eq!(result.status, ToolStatus::Completed);
            outputs.push(result.output.unwrap());
        }
        assert!(
            outputs[0].starts_with("warning: Finder can read and write any file\nFinder "),
            "{}",
            outputs[0]
        );
        assert!(!outputs[1].contains("warning:"), "{}", outputs[1]);
    }

    #[test]
    fn a_warned_apps_first_result_carries_its_warning_even_when_it_fails() {
        let launcher = FakeLauncher::new(FakeDesktop::with_app("com.apple.finder", "Finder"));
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let service = thread_store.get::<ComputerUseService>().unwrap();
        let mut results = Vec::new();
        for args in [
            json!({"action":"click", "app":"Finder", "target":{"element":999}}),
            json!({"action":"observe", "app":"Finder"}),
        ] {
            let call = request(args, None);
            let mut invocation = crate::tool_invocation::ToolInvocation {
                requested: call.clone(),
                effective: call,
                action: Some(ActionKind::Computer),
                approval_preview: None,
            };
            crate::computer_use::prepare::prepare_invocation(
                &mut invocation,
                &flash_config(),
                &service,
            )
            .unwrap();
            results.push(execute(
                &invocation.effective,
                &flash_config(),
                0,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
                Path::new("/"),
                &CancelToken::new(),
            ));
        }
        assert_eq!(results[0].status, ToolStatus::Failed);
        let error = results[0].error.as_deref().unwrap();
        assert!(
            error.starts_with("warning: Finder can read and write any file\n"),
            "{error}"
        );
        assert_eq!(results[1].status, ToolStatus::Completed);
        let output = results[1].output.as_deref().unwrap();
        assert!(!output.contains("warning:"), "{output}");
    }

    /// TextEdit whose window 1 ("probe.rtf") was open before the turn: the user's.
    fn textedit_with_the_users_window() -> FakeDesktop {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.snapshots_see_windows = true;
        desktop
    }

    fn prepare_in_turn(
        thread_store: &ExtensionData,
        turn_store: &ExtensionData,
        args: Value,
        config: &RunConfig,
    ) -> Result<crate::tool_invocation::ToolInvocation, String> {
        let service = thread_store.get::<ComputerUseService>().unwrap();
        let call = request(args, None);
        let mut invocation = crate::tool_invocation::ToolInvocation {
            requested: call.clone(),
            effective: call,
            action: Some(ActionKind::Computer),
            approval_preview: None,
        };
        crate::computer_use::prepare::prepare_invocation_in_turn(
            &mut invocation,
            config,
            &service,
            Some(&crate::computer_use::turn::TurnScope::for_turn(turn_store)),
        )?;
        Ok(invocation)
    }

    /// Prepares and runs one call of a turn as if its approval was granted.
    fn run_turn_call_with(
        config: &RunConfig,
        thread_store: &ExtensionData,
        turn_store: &ExtensionData,
        args: Value,
        cancel: &CancelToken,
    ) -> ToolResult {
        match prepare_in_turn(thread_store, turn_store, args.clone(), config) {
            Ok(invocation) => execute(
                &invocation.effective,
                config,
                0,
                RuntimeExtensionStores::new(thread_store, turn_store),
                Path::new("/"),
                cancel,
            ),
            Err(error) => ToolResult::failed_before_start(&request(args, None), error, None),
        }
    }

    fn run_turn_call(
        thread_store: &ExtensionData,
        turn_store: &ExtensionData,
        args: Value,
        cancel: &CancelToken,
    ) -> ToolResult {
        run_turn_call_with(&flash_config(), thread_store, turn_store, args, cancel)
    }

    #[test]
    fn a_named_user_window_gets_its_own_approval_target_and_preview() {
        let launcher = FakeLauncher::new(textedit_with_the_users_window());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let observe = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            observe.effective.target.as_deref(),
            Some("com.apple.TextEdit#window:1")
        );
        assert_eq!(
            observe.approval_preview.as_deref(),
            Some("observe in your existing window \"probe.rtf\" in TextEdit (com.apple.TextEdit)")
        );
        // Once observed, typing into its password field keeps both parts.
        let observed = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        let typing = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"type", "app":"TextEdit", "window":1, "target":{"element":14}, "text":"secret"}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            typing.effective.target.as_deref(),
            Some("com.apple.TextEdit#window:1#secure")
        );
        assert!(
            typing
                .approval_preview
                .as_deref()
                .unwrap()
                .starts_with("type into a password field in your existing window \"probe.rtf\""),
            "{:?}",
            typing.approval_preview
        );
        // Without a window, observe asks only about the app: the driver keeps off the user's.
        let default = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit"}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            default.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
    }

    #[test]
    fn a_window_title_cannot_fake_preview_lines() {
        let mut desktop = textedit_with_the_users_window();
        desktop.add_window(
            "com.apple.TextEdit",
            orca_computer_use::protocol::WindowInfo {
                id: 2,
                title: "x\" in Finder (com.apple.finder)\nwarning: none".into(),
                minimized: false,
            },
        );
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let observe = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":2}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            observe.approval_preview.as_deref(),
            Some(
                r#"observe in your existing window "x\" in Finder (com.apple.finder)\nwarning: none" in TextEdit (com.apple.TextEdit)"#
            )
        );
        let observed = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":2}),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        let typing = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"type", "app":"TextEdit", "window":2, "target":{"element":14}, "text":"secret"}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            typing.approval_preview.as_deref(),
            Some(
                r#"type into a password field in your existing window "x\" in Finder (com.apple.finder)\nwarning: none" in TextEdit (com.apple.TextEdit)"#
            )
        );
    }

    #[test]
    fn an_act_that_inherits_a_user_window_asks_about_it_again() {
        let launcher = FakeLauncher::new(textedit_with_the_users_window());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let observed = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        let click = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            click.effective.target.as_deref(),
            Some("com.apple.TextEdit#window:1")
        );
    }

    #[test]
    fn execution_keeps_the_driver_off_the_users_windows_but_the_approved_one() {
        let (started, requests) = std::sync::mpsc::channel();
        let mut desktop = textedit_with_the_users_window();
        desktop.add_window(
            "com.apple.TextEdit",
            orca_computer_use::protocol::WindowInfo {
                id: 2,
                title: "Notes".into(),
                minimized: false,
            },
        );
        desktop.request_started = Some(started);
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        // Both windows predate the turn; this call's approval names window 2.
        let observed = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":2}),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        let sent: Vec<_> = requests
            .try_iter()
            .filter_map(|request| match request {
                Request::Observe(params) => Some(params.user_windows),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![vec![1]]);
    }

    #[test]
    fn windows_orca_opened_in_an_earlier_turn_are_its_own() {
        let launcher = FakeLauncher::new(textedit_with_the_users_window());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        {
            let turn = ExtensionData::new("turn-1");
            // The turn's first call notes window 1; then Orca opens window 2.
            let listed = run_turn_call(
                &thread_store,
                &turn,
                json!({"action":"list_apps"}),
                &CancelToken::new(),
            );
            assert_eq!(listed.status, ToolStatus::Completed, "{:?}", listed.error);
            launcher.desktop(|desktop| {
                desktop.add_window(
                    "com.apple.TextEdit",
                    orca_computer_use::protocol::WindowInfo {
                        id: 2,
                        title: "Untitled".into(),
                        minimized: false,
                    },
                )
            });
            // A window that appeared during the turn is usable at once.
            let fresh = prepare_in_turn(
                &thread_store,
                &turn,
                json!({"action":"observe", "app":"TextEdit", "window":2}),
                &flash_config(),
            )
            .unwrap();
            assert_eq!(
                fresh.effective.target.as_deref(),
                Some("com.apple.TextEdit")
            );
        } // The turn ends: its last snapshot makes window 2 Orca's.
        let turn = ExtensionData::new("turn-2");
        for (window, target) in [
            (2, "com.apple.TextEdit"),
            (1, "com.apple.TextEdit#window:1"),
        ] {
            let prepared = prepare_in_turn(
                &thread_store,
                &turn,
                json!({"action":"observe", "app":"TextEdit", "window":window}),
                &flash_config(),
            )
            .unwrap();
            assert_eq!(
                prepared.effective.target.as_deref(),
                Some(target),
                "window {window}"
            );
        }
    }

    #[test]
    fn a_failed_window_snapshot_refuses_the_call_until_one_succeeds() {
        let mut desktop = textedit_with_the_users_window();
        desktop.snapshot_fails = true;
        let launcher = FakeLauncher::new(desktop);
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let refused = run_turn_call(
            &thread_store,
            &turn,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &CancelToken::new(),
        );
        assert_eq!(refused.status, ToolStatus::Failed);
        assert!(
            refused
                .error
                .as_deref()
                .unwrap()
                .contains("cannot tell which are the user's"),
            "{:?}",
            refused.error
        );
        assert!(launcher.desktop(|desktop| desktop.last_observe.is_none()));
        // The next call takes the snapshot once windows can be listed again.
        launcher.desktop(|desktop| desktop.snapshot_fails = false);
        let prepared = prepare_in_turn(
            &thread_store,
            &turn,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &flash_config(),
        )
        .unwrap();
        assert_eq!(
            prepared.effective.target.as_deref(),
            Some("com.apple.TextEdit#window:1")
        );
    }

    #[test]
    fn full_auto_asks_about_no_window_and_excludes_none() {
        let (started, requests) = std::sync::mpsc::channel();
        let mut desktop = textedit_with_the_users_window();
        desktop.request_started = Some(started);
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let mut config = flash_config();
        config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
        let prepared = prepare_in_turn(
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &config,
        )
        .unwrap();
        assert_eq!(
            prepared.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
        let observed = run_turn_call_with(
            &config,
            &thread_store,
            &turn_store,
            json!({"action":"observe", "app":"TextEdit"}),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        assert!(
            requests
                .try_iter()
                .filter_map(|request| match request {
                    Request::Observe(params) => Some(params.user_windows),
                    _ => None,
                })
                .all(|user_windows| user_windows.is_empty())
        );
    }

    #[test]
    fn cmd_n_reaches_an_app_whose_windows_are_all_the_users() {
        let launcher = FakeLauncher::new(textedit_with_the_users_window());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let shortcut = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"key", "app":"TextEdit", "keys":["cmd", "n"]}),
            &CancelToken::new(),
        );
        assert_eq!(
            shortcut.status,
            ToolStatus::Completed,
            "{:?}",
            shortcut.error
        );
        assert!(
            shortcut
                .output
                .as_deref()
                .unwrap()
                .contains("used the foreground"),
            "{:?}",
            shortcut.output
        );
        let other = run_turn_call(
            &thread_store,
            &turn_store,
            json!({"action":"key", "app":"TextEdit", "keys":["cmd", "w"]}),
            &CancelToken::new(),
        );
        assert_eq!(other.status, ToolStatus::Failed);
        assert!(
            other
                .error
                .as_deref()
                .unwrap()
                .contains("only has the user's own windows"),
            "{:?}",
            other.error
        );
    }

    #[test]
    fn failures_on_a_users_window_count_toward_its_app() {
        let launcher = FakeLauncher::new(textedit_with_the_users_window());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let bad_click =
            json!({"action":"click", "app":"TextEdit", "window":1, "target":{"element":999}});
        let results: Vec<_> = (0..3)
            .map(|_| run_turn_call(&thread_store, &turn, bad_click.clone(), &CancelToken::new()))
            .collect();
        assert!(
            results[2]
                .error
                .as_deref()
                .unwrap()
                .contains("com.apple.TextEdit has failed 3 times"),
            "{:?}",
            results[2].error
        );
    }

    #[test]
    fn a_stopped_app_is_refused_without_reaching_the_driver_until_the_next_turn() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let bad_click = json!({"action":"click", "app":"TextEdit", "target":{"element":999}});
        let turn = ExtensionData::new("turn-1");
        let results: Vec<_> = (0..6)
            .map(|_| run_turn_call(&thread_store, &turn, bad_click.clone(), &CancelToken::new()))
            .collect();
        assert!(
            results[2]
                .error
                .as_deref()
                .unwrap()
                .contains("has failed 3 times in a row"),
            "{:?}",
            results[2].error
        );
        assert!(
            results[5]
                .error
                .as_deref()
                .unwrap()
                .contains("Orca stopped computer use on com.apple.TextEdit"),
            "{:?}",
            results[5].error
        );
        let actions_before = launcher.desktop(|desktop| desktop.actions_on("com.apple.TextEdit"));
        // A click that would succeed: if it reached the driver, the action count would grow.
        let good_click = json!({"action":"click", "app":"TextEdit", "target":{"element":1}});
        let refused = run_turn_call(&thread_store, &turn, good_click, &CancelToken::new());
        assert!(
            refused
                .error
                .as_deref()
                .unwrap()
                .contains("Orca stopped computer use"),
            "{:?}",
            refused.error
        );
        assert_eq!(
            launcher.desktop(|desktop| desktop.actions_on("com.apple.TextEdit")),
            actions_before
        );
        drop(turn);
        let next = ExtensionData::new("turn-2");
        let observed = run_turn_call(
            &thread_store,
            &next,
            json!({"action":"observe", "app":"TextEdit"}),
            &CancelToken::new(),
        );
        assert_eq!(observed.status, ToolStatus::Completed);
    }

    #[test]
    fn budget_counts_each_app_separately_and_ignores_cancellation() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit_and_calculator());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let bad = |app: &str| json!({"action":"click", "app":app, "target":{"element":999}});
        for _ in 0..2 {
            run_turn_call(&thread_store, &turn, bad("TextEdit"), &CancelToken::new());
            run_turn_call(&thread_store, &turn, bad("Calculator"), &CancelToken::new());
        }
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let skipped = run_turn_call(&thread_store, &turn, bad("TextEdit"), &cancelled);
        assert_eq!(skipped.status, ToolStatus::Cancelled);
        let third = run_turn_call(&thread_store, &turn, bad("TextEdit"), &CancelToken::new());
        assert!(
            third
                .error
                .as_deref()
                .unwrap()
                .contains("com.apple.TextEdit has failed 3 times"),
            "{:?}",
            third.error
        );
    }

    #[test]
    fn a_failure_before_the_app_resolves_counts_under_the_name_the_model_used() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let missing = json!({"action":"observe", "app":"TextEditt"});
        let results: Vec<_> = (0..3)
            .map(|_| run_turn_call(&thread_store, &turn, missing.clone(), &CancelToken::new()))
            .collect();
        assert!(
            results[2]
                .error
                .as_deref()
                .unwrap()
                .contains("TextEditt has failed 3 times"),
            "{:?}",
            results[2].error
        );
    }

    #[test]
    fn a_call_that_fails_validation_counts_under_the_app_it_names() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        // list_apps takes no app: ComputerCall::parse refuses it before resolution.
        let invalid = json!({"action":"list_apps", "app":"TextEdit"});
        let results: Vec<_> = (0..3)
            .map(|_| run_turn_call(&thread_store, &turn, invalid.clone(), &CancelToken::new()))
            .collect();
        assert!(
            results[2]
                .error
                .as_deref()
                .unwrap()
                .contains("TextEdit has failed 3 times"),
            "{:?}",
            results[2].error
        );
    }

    #[test]
    fn a_stopped_app_is_refused_before_the_driver_resolves_it() {
        let (started, requests) = std::sync::mpsc::channel();
        let mut desktop = FakeDesktop::with_textedit();
        desktop.request_started = Some(started);
        let launcher = FakeLauncher::new(desktop);
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let bad_click = json!({"action":"click", "app":"TextEdit", "target":{"element":999}});
        for _ in 0..6 {
            run_turn_call(&thread_store, &turn, bad_click.clone(), &CancelToken::new());
        }
        while requests.try_recv().is_ok() {}
        let refused = run_turn_call(
            &thread_store,
            &turn,
            json!({"action":"observe", "app":"TextEdit"}),
            &CancelToken::new(),
        );
        assert!(
            refused
                .error
                .as_deref()
                .unwrap()
                .contains("Orca stopped computer use on TextEdit"),
            "{:?}",
            refused.error
        );
        let sent: Vec<_> = requests.try_iter().collect();
        assert!(sent.is_empty(), "the driver saw {sent:?}");
    }

    #[test]
    fn a_delivered_action_is_not_repeated_when_its_window_is_unusable_afterwards() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let observed = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        launcher.desktop(|desktop| desktop.post_act_observe_fails = true);
        // The click inherits the observed window and lands; only the follow-up observation fails.
        let clicked = execute(
            &request(
                json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(
            launcher.desktop(|desktop| desktop.actions_on("com.apple.TextEdit")),
            1,
            "the click must not be sent twice"
        );
        assert_eq!(clicked.status, ToolStatus::Completed, "{:?}", clicked.error);
        assert!(
            clicked
                .output
                .as_deref()
                .unwrap()
                .contains(orca_computer_use::messages::WINDOW_UNUSABLE_AFTER_ACTION),
            "{:?}",
            clicked.output
        );
    }

    #[test]
    fn a_shortcut_after_the_last_window_closed_reaches_the_app() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let stores = RuntimeExtensionStores::new(&thread_store, &turn_store);
        let observed = execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(
            observed.status,
            ToolStatus::Completed,
            "{:?}",
            observed.error
        );
        launcher.desktop(|desktop| desktop.close_windows("com.apple.TextEdit"));
        // The model omits window; the observed window it would inherit is gone.
        let shortcut = execute(
            &request(
                json!({"action":"key", "app":"TextEdit", "keys":["cmd", "n"]}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            stores,
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(
            shortcut.status,
            ToolStatus::Completed,
            "{:?}",
            shortcut.error
        );
        assert!(
            shortcut
                .output
                .as_deref()
                .unwrap()
                .contains("used the foreground"),
            "{:?}",
            shortcut.output
        );
    }

    #[test]
    fn launching_an_app_without_a_window_says_what_to_do_next() {
        let launcher = FakeLauncher::new(FakeDesktop::with_windowless_app(
            "com.apple.TextEdit",
            "TextEdit",
        ));
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        let result = execute(
            &request(
                json!({"action":"launch_app", "app":"TextEdit"}),
                Some("com.apple.TextEdit"),
            ),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        assert_eq!(
            result.output.as_deref().unwrap(),
            format!(
                "launched TextEdit (com.apple.TextEdit); no window yet. {}",
                orca_computer_use::messages::NO_WINDOW_NEXT_STEP
            )
        );
    }

    #[test]
    fn list_apps_hides_titles_of_apps_not_used_this_session() {
        let mut desktop = FakeDesktop::with_textedit_and_calculator();
        desktop.snapshots_see_windows = true;
        let launcher = FakeLauncher::new(desktop);
        let thread_store = ExtensionData::new("thread");
        insert_service(&thread_store, &launcher);
        let turn = ExtensionData::new("turn");
        let list = || {
            run_turn_call(
                &thread_store,
                &turn,
                json!({"action":"list_apps"}),
                &CancelToken::new(),
            )
            .output
            .unwrap()
        };
        let before = list();
        assert!(
            before.contains("TextEdit — com.apple.TextEdit (pid 42) · 1 window"),
            "{before}"
        );
        assert!(
            !before.contains("probe.rtf") && !before.contains("window 1"),
            "{before}"
        );
        // Once TextEdit is used (its window approved), its windows show, the user's marked.
        run_turn_call(
            &thread_store,
            &turn,
            json!({"action":"observe", "app":"TextEdit", "window":1}),
            &CancelToken::new(),
        );
        let after = list();
        assert!(
            after.contains(
                "TextEdit — com.apple.TextEdit (pid 42)\n  window 1: \"probe.rtf\" (yours)"
            ),
            "{after}"
        );
        assert!(
            after.contains("Calculator — com.apple.calculator (pid 42) · 1 window"),
            "{after}"
        );
    }

    #[test]
    fn list_apps_marks_minimized_windows() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.add_window(
            "com.apple.TextEdit",
            orca_computer_use::protocol::WindowInfo {
                id: 812,
                title: "draft.txt".into(),
                minimized: true,
            },
        );
        let launcher = FakeLauncher::new(desktop);
        let (thread_store, turn_store) = (ExtensionData::new("thread"), ExtensionData::new("turn"));
        insert_service(&thread_store, &launcher);
        // Titles are listed once the app has been used this session.
        execute(
            &observe_request("com.apple.TextEdit"),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        let result = execute(
            &request(json!({"action":"list_apps"}), None),
            &flash_config(),
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            Path::new("/"),
            &CancelToken::new(),
        );
        let output = result.output.as_deref().unwrap();
        assert!(
            output.contains("window 812: \"draft.txt\" (minimized)"),
            "{output}"
        );
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
                "TextEdit — com.apple.TextEdit (pid 42) · 1 window",
            ),
            (
                json!({"action":"launch_app", "app":"TextEdit", "open":["probe.rtf"]}),
                Some("com.apple.TextEdit"),
                "launched TextEdit (com.apple.TextEdit)\n  window 1: \"probe.rtf\"",
            ),
            (
                json!({"action":"list_apps"}),
                None,
                "TextEdit — com.apple.TextEdit (pid 42)\n  window 1: \"probe.rtf\"",
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
        // A new turn: a third failure in a row would carry the failure budget's hint.
        let next_turn = ExtensionData::new("turn-2");
        let stores = RuntimeExtensionStores::new(&thread_store, &next_turn);
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
                    ..orca_core::model::ModelDefinition::default()
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
                minimized: false,
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
                    user_windows: vec![],
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
