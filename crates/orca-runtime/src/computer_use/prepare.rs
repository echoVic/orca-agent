use std::time::Duration;

use orca_computer_use::apps::{RiskTier, risk_tier};
use orca_computer_use::call::ComputerCall;
use orca_computer_use::client::CallError;
use orca_computer_use::protocol::{Action, ErrorKind, Reply, Request, Target};
use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
use orca_core::config::RunConfig;

use super::ComputerUseService;
use super::budget::FailureBudget;
use crate::tool_invocation::ToolInvocation;

/// Resolve the app and apply authorization restrictions before asking approval.
/// Tests that do not exercise the failure budget use this form.
#[cfg(test)]
pub(crate) fn prepare_invocation(
    invocation: &mut ToolInvocation,
    config: &RunConfig,
    service: &ComputerUseService,
) -> Result<(), String> {
    prepare_invocation_with_budget(invocation, config, service, None)
}

/// As `prepare_invocation`, also refusing an app the turn's failure budget
/// stopped (before any approval is asked) and counting failures found here.
pub(crate) fn prepare_invocation_with_budget(
    invocation: &mut ToolInvocation,
    config: &RunConfig,
    service: &ComputerUseService,
    budget: Option<&FailureBudget>,
) -> Result<(), String> {
    let raw = super::budget::raw_app(invocation.effective.raw_arguments.as_deref());
    // A known name of a stopped app is refused before the driver resolves it.
    if let (Some(budget), Some(raw)) = (budget, raw.as_deref())
        && let Some(stop) = budget.stopped(raw)
    {
        return Err(stop);
    }
    let mut app_key = raw.clone();
    let result = prepare(invocation, config, service, &mut app_key);
    let (Some(budget), Some(app)) = (budget, app_key.as_deref()) else {
        return result;
    };
    if let Some(raw) = raw.as_deref()
        && raw != app
    {
        budget.alias(raw, app);
    }
    if let Some(stop) = budget.stopped(app) {
        return Err(stop);
    }
    result.map_err(|mut message| {
        if let Some(note) = budget.record(app, true) {
            message.push('\n');
            message.push_str(&note);
        }
        message
    })
}

/// `app_key` names the app as soon as it is known: the model's name first, the
/// resolved bundle ID once resolution succeeds.
fn prepare(
    invocation: &mut ToolInvocation,
    config: &RunConfig,
    service: &ComputerUseService,
    app_key: &mut Option<String>,
) -> Result<(), String> {
    ensure_visible(config, 0, Some(service))?;
    let arguments = serde_json::from_str(
        invocation
            .effective
            .raw_arguments
            .as_deref()
            .unwrap_or("{}"),
    )
    .map_err(|error| error.to_string())?;
    let call = ComputerCall::parse(&arguments)?;
    *app_key = call.app().map(str::to_owned).or(app_key.take());
    if config.approval_mode == ApprovalMode::Plan
        && !matches!(call, ComputerCall::ListApps | ComputerCall::Observe { .. })
    {
        return Err("plan mode only allows list_apps and observe".into());
    }
    let hello = check_permissions(service)?;
    if matches!(call, ComputerCall::ListApps) {
        invocation.action = None;
        return Ok(());
    }
    let app_name = call.app().expect("app call");
    let resolved = service.call(
        Request::ResolveApp {
            app: app_name.to_owned(),
            allow_not_running: matches!(call, ComputerCall::LaunchApp { .. }),
        },
        Duration::from_secs(30),
    );
    let app = match resolved {
        Ok(Reply::Resolved { app }) => app,
        Err(CallError::Driver(error)) if error.kind == ErrorKind::AppAmbiguous => {
            let candidates = error
                .candidates
                .iter()
                .map(|app| {
                    format!(
                        "  {} — {} (pid {})",
                        app.name,
                        app.bundle_id,
                        app.pid
                            .map_or_else(|| "not running".into(), |pid| pid.to_string())
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "\"{app_name}\" matches several apps; use a bundle id:\n{candidates}"
            ));
        }
        Err(CallError::Driver(error)) if error.kind == ErrorKind::AppNotFound => {
            return Err(format!("no app matches \"{app_name}\"; call list_apps"));
        }
        Err(CallError::Driver(error)) => return Err(error.message),
        Err(CallError::TimedOut) => return Err("Computer-use driver request timed out".into()),
        Err(CallError::Disconnected) => return Err("Computer-use driver disconnected".into()),
        Ok(_) => return Err("Computer-use driver did not reply to resolve_app".into()),
    };
    *app_key = Some(app.bundle_id.clone());
    let warning = match risk_tier(
        &app.bundle_id,
        app.category.as_deref(),
        hello.host_bundle_id.as_deref(),
    ) {
        RiskTier::HostTerminal => {
            return Err(format!(
                "Orca cannot operate the terminal it runs in ({}).",
                app.name
            ));
        }
        RiskTier::Forbidden => {
            let allowed = config.permission_rules.rules.iter().any(|rule| {
                rule.tool == "computer"
                    && rule.pattern.as_deref() == Some(app.bundle_id.as_str())
                    && rule.decision == Decision::Allow
            });
            if !allowed {
                return Err(format!(
                    "{} is blocked by default. To allow it, add to your config:\n[[permissions.rules]]\ntool = \"computer\"\npattern = \"{}\"\ndecision = \"allow\"",
                    app.name, app.bundle_id
                ));
            }
            None
        }
        RiskTier::Warning(text) => Some(format!("warning: {} {text}", app.name)),
        RiskTier::Normal => None,
    };
    let secure = match &call {
        ComputerCall::Act { window, action, .. } => {
            let target = match action {
                Action::Type { target, .. } => Some(target),
                Action::Key { target, keys }
                    if orca_computer_use::keys::parse_combo(keys)?.enters_text() =>
                {
                    Some(target)
                }
                _ => None,
            };
            // Match execution: explicit windows win, otherwise use the latest observation.
            let window = window.or_else(|| service.latest_window(&app.bundle_id));
            match target {
                Some(Some(Target::Element { element })) => {
                    service.is_secure(&app.bundle_id, window, *element)
                }
                Some(None) => service.focused_secure(&app.bundle_id, window),
                _ => false,
            }
        }
        _ => false,
    };
    invocation.action = Some(ActionKind::Computer);
    invocation.effective.target = Some(if secure {
        format!("{}#secure", app.bundle_id)
    } else {
        app.bundle_id.clone()
    });
    let mut preview = if secure {
        format!(
            "{} into a password field in {} ({})",
            call.action_name(),
            app.name,
            app.bundle_id
        )
    } else {
        format!("{} in {} ({})", call.action_name(), app.name, app.bundle_id)
    };
    if let ComputerCall::LaunchApp { open, .. } = &call {
        for path in open {
            preview.push_str("\nopen: ");
            preview.push_str(&serde_json::to_string(path).expect("string serialization"));
        }
    }
    if let Some(warning) = warning {
        service.note_risk_warning(&app.bundle_id, &warning);
        preview.push('\n');
        preview.push_str(&warning);
    }
    invocation.approval_preview = Some(preview);
    Ok(())
}

/// Enforce the provider's availability predicate again at the execution boundary.
pub(crate) fn ensure_visible(
    config: &RunConfig,
    depth: u32,
    service: Option<&ComputerUseService>,
) -> Result<(), String> {
    let host_supported = orca_computer_use::apps::host_supported();
    #[cfg(any(test, feature = "test-utils"))]
    let host_supported = service.map_or(host_supported, ComputerUseService::test_host_supported);
    #[cfg(not(any(test, feature = "test-utils")))]
    let _ = service;
    if !super::visibility::tool_visible(
        config,
        depth,
        super::visibility::configured_tools(config),
        host_supported,
    ) {
        return Err("computer use is only available in the interactive TUI on macOS 14 or later; this session does not expose computer (main session only, no custom agent, and no whole-tool deny rule)".into());
    }
    Ok(())
}

/// Check before arming the Esc listener, including for list_apps.
pub(crate) fn check_permissions(
    service: &ComputerUseService,
) -> Result<orca_computer_use::client::HelloInfo, String> {
    let hello = service.hello().map_err(|error| error.message)?;
    let mut missing = Vec::new();
    if !hello.permissions.accessibility || !hello.permissions.event_tap {
        missing.push("Accessibility");
    }
    if !hello.permissions.screen_recording {
        missing.push("Screen Recording");
    }
    if !missing.is_empty() {
        // Release the machine-wide lock and avoid caching a denied handshake.
        service.reset();
        return Err(format!(
            "computer use needs {} for the app this terminal runs in ({}). Turn it on in System Settings > Privacy & Security; after granting Screen Recording, restart the terminal.",
            missing.join(" and "),
            hello.host_bundle_id.as_deref().unwrap_or("your terminal")
        ));
    }
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use orca_computer_use::fake::{FakeDesktop, FakeLauncher};
    use orca_computer_use::protocol::{ObserveParams, Reply, Request};
    use orca_core::approval_rules::PermissionRule;
    use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
    use orca_core::config::{ComputerUseConfig, RunConfig};
    use orca_core::tool_types::{ToolName, ToolRequest};
    use serde_json::{Value, json};

    use super::prepare_invocation;
    use crate::computer_use::ComputerUseService;
    use crate::tool_invocation::ToolInvocation;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn native_secure_preview_tiers_and_provider_type_schema() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let home = std::env::var_os("ORCA_HOME").expect("isolated native ORCA_HOME");
        assert!(std::path::Path::new(&home).starts_with("/tmp"));
        let service =
            ComputerUseService::new_for_test(Arc::new(crate::computer_use::ProcessLauncher {
                exe: std::env::var_os("ORCA_CU_DRIVER_EXE")
                    .expect("native driver executable")
                    .into(),
                cwd: directory.path().into(),
                approval_mode: ApprovalMode::Suggest,
            }));
        let registry = orca_tools::registry::default_tool_registry();
        let spec = registry.get("computer").unwrap().spec();
        let wire = orca_provider::tool_schema::deepseek_tools_schema(&[
            orca_provider::tool_schema::ProviderToolDefinition {
                name: "computer".into(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema.clone(),
                strict_capable: false,
            },
        ]);
        let description = wire[0]["function"]["parameters"]["properties"]["text"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("text only")
                && description.contains("multi-line")
                && description.contains("key")
                && description.contains("refused"),
            "{description}"
        );
        eprintln!("model-facing provider schema BEFORE approval: {description}");
        let Reply::Resolved { app } = service
            .call(
                Request::ResolveApp {
                    app: "dev.orca.cu-fixture".into(),
                    allow_not_running: false,
                },
                Duration::from_secs(30),
            )
            .unwrap()
        else {
            panic!("real app resolution")
        };
        let Reply::Observed { observation } = service
            .call(
                Request::Observe(ObserveParams {
                    app: app.bundle_id.clone(),
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
            panic!("real observation")
        };
        assert!(
            observation
                .elements
                .iter()
                .any(|e| e.secure && e.label.as_deref() == Some("Password"))
        );
        let element = observation
            .elements
            .iter()
            .find(|e| e.secure && e.label.as_deref() == Some("Password"))
            .unwrap()
            .id;
        service.record_observation(&observation);
        let (_, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"type","app":app.bundle_id,"text":"preview-only","target":{"element":element}}),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("dev.orca.cu-fixture#secure")
        );
        let preview = invocation.approval_preview.unwrap();
        assert!(
            preview.contains(&format!("{} ({})", app.name, app.bundle_id)),
            "{preview}"
        );
        eprintln!(
            "native secure-entry approval preview: {preview}; canonical target={:?}; no act was executed",
            invocation.effective.target
        );
        for (bundle, blocked) in [("COM.APPLE.FINDER", false), ("COM.APPLE.PASSWORDS", true)] {
            let (_, mut invocation) = setup(
                FakeDesktop::with_textedit(),
                json!({"action":"observe","app":bundle}),
            );
            let result = prepare_invocation(&mut invocation, &interactive_config(), &service);
            if blocked {
                let error = result.unwrap_err();
                assert!(error.contains("blocked by default"), "{error}");
                eprintln!("native case-variant forbidden tier: {bundle}, {error}");
            } else {
                result.unwrap();
                let preview = invocation.approval_preview.unwrap();
                assert!(preview.contains("warning:"), "{preview}");
                assert!(preview.contains(bundle));
                eprintln!("native case-variant warning tier and canonical preview: {preview}");
            }
        }
    }

    fn setup(desktop: FakeDesktop, args: Value) -> (ComputerUseService, ToolInvocation) {
        let request = ToolRequest {
            id: "computer-1".into(),
            name: ToolName::Computer,
            action: ActionKind::Computer,
            target: None,
            raw_arguments: Some(args.to_string()),
        };
        (
            ComputerUseService::new_for_test(Arc::new(FakeLauncher::new(desktop))),
            ToolInvocation {
                requested: request.clone(),
                effective: request,
                action: Some(ActionKind::Computer),
                approval_preview: None,
            },
        )
    }

    fn interactive_config() -> RunConfig {
        RunConfig {
            computer_use: ComputerUseConfig {
                interactive: true,
                ..Default::default()
            },
            ..RunConfig::default()
        }
    }

    #[test]
    fn a_name_resolves_to_its_bundle_id_as_the_approval_target() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"observe", "app":"TextEdit"}),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
        assert_eq!(invocation.action, Some(ActionKind::Computer));
        assert_eq!(
            invocation.approval_preview.as_deref(),
            Some("observe in TextEdit (com.apple.TextEdit)")
        );
    }

    #[test]
    fn an_ambiguous_app_name_lists_candidates_without_asking() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_two_apps_named("Code"),
            json!({"action":"observe", "app":"Code"}),
        );
        assert_eq!(
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err(),
            "\"Code\" matches several apps; use a bundle id:\n  Code — com.microsoft.VSCode (pid 42)\n  Code — com.todesktop.230313mzl4w4u92 (pid 42)"
        );
        assert_eq!(invocation.effective.target, None);
    }

    #[test]
    fn a_password_manager_needs_an_exact_rule() {
        let desktop = FakeDesktop::with_app("com.1password.1password", "1Password");
        let args = json!({"action":"observe", "app":"com.1password.1password"});
        let mut config = interactive_config();
        config
            .permission_rules
            .rules
            .push(PermissionRule::whole_tool("computer", Decision::Allow));
        config.permission_rules.rules.push(PermissionRule::new(
            "computer",
            "com.1password.*",
            Decision::Allow,
        ));
        let (service, mut invocation) = setup(desktop.clone(), args.clone());
        assert_eq!(
            prepare_invocation(&mut invocation, &config, &service).unwrap_err(),
            "1Password is blocked by default. To allow it, add to your config:\n[[permissions.rules]]\ntool = \"computer\"\npattern = \"com.1password.1password\"\ndecision = \"allow\""
        );
        config.permission_rules.rules.push(PermissionRule::new(
            "computer",
            "com.1password.1password",
            Decision::Prompt,
        ));
        assert!(prepare_invocation(&mut invocation, &config, &service).is_err());
        config.permission_rules.rules.push(PermissionRule::new(
            "computer",
            "com.1password.1password",
            Decision::Allow,
        ));
        let (service, mut invocation) = setup(desktop, args);
        prepare_invocation(&mut invocation, &config, &service).unwrap();
    }

    #[test]
    fn the_host_terminal_is_refused() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_app("com.cmuxterm.app", "cmux"),
            json!({"action":"observe", "app":"cmux"}),
        );
        let mut config = interactive_config();
        config.permission_rules.rules.push(PermissionRule::new(
            "computer",
            "com.cmuxterm.app",
            Decision::Allow,
        ));
        assert_eq!(
            prepare_invocation(&mut invocation, &config, &service).unwrap_err(),
            "Orca cannot operate the terminal it runs in (cmux)."
        );
    }

    #[test]
    fn plan_mode_only_allows_list_apps_and_observe() {
        let mut config = interactive_config();
        config.approval_mode = ApprovalMode::Plan;
        for arguments in [
            json!({"action":"click", "app":"TextEdit", "target":{"element":1}}),
            json!({"action":"type", "app":"TextEdit", "text":"test"}),
            json!({"action":"key", "app":"TextEdit", "keys":["a"]}),
            json!({"action":"scroll", "app":"TextEdit", "target":{"element":1}, "direction":"up"}),
            json!({"action":"launch_app", "app":"TextEdit"}),
        ] {
            let mut desktop = FakeDesktop::with_textedit();
            desktop.permissions.accessibility = false;
            let (service, mut invocation) = setup(desktop, arguments);
            assert_eq!(
                prepare_invocation(&mut invocation, &config, &service).unwrap_err(),
                "plan mode only allows list_apps and observe"
            );
        }
        for arguments in [
            json!({"action":"observe", "app":"TextEdit"}),
            json!({"action":"list_apps"}),
        ] {
            let (service, mut invocation) = setup(FakeDesktop::with_textedit(), arguments);
            prepare_invocation(&mut invocation, &config, &service).unwrap();
        }
    }

    #[test]
    fn list_apps_needs_no_approval() {
        let desktop = FakeDesktop::with_textedit();
        let (service, mut invocation) = setup(desktop, json!({"action":"list_apps"}));
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(invocation.action, None);
    }

    #[test]
    fn a_warning_app_gets_the_warning_in_the_preview() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_app("com.apple.Safari", "Safari"),
            json!({"action":"observe", "app":"Safari"}),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.approval_preview.as_deref(),
            Some(
                "observe in Safari (com.apple.Safari)\nwarning: Safari web pages may carry instructions aimed at the agent"
            )
        );
    }

    #[test]
    fn typing_into_a_password_field_asks_separately() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"type", "app":"TextEdit", "text":"secret", "target":{"element":14}}),
        );
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
        service.record_observation(&observation);
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit#secure")
        );
        assert_eq!(
            invocation.approval_preview.as_deref(),
            Some("type into a password field in TextEdit (com.apple.TextEdit)")
        );
        assert_eq!(invocation.action, Some(ActionKind::Computer));
        observation.focused_secure = true;
        service.record_observation(&observation);
        invocation.effective.raw_arguments =
            Some(json!({"action":"type", "app":"TextEdit", "text":"secret"}).to_string());
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit#secure")
        );
        invocation.effective.raw_arguments = Some(
            json!({"action":"type", "app":"TextEdit", "text":"test", "target":{"element":2}})
                .to_string(),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
    }

    #[test]
    fn an_explicit_window_checks_its_own_secure_elements_before_approval() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.add_window(
            "com.apple.TextEdit",
            orca_computer_use::protocol::WindowInfo {
                id: 2,
                title: "B".into(),
                minimized: false,
            },
        );
        let (service, mut invocation) = setup(
            desktop,
            json!({"action":"type", "app":"TextEdit", "window":1, "target":{"element":14}, "text":"secret"}),
        );
        for window in [1, 2] {
            let Reply::Observed { mut observation } = service
                .call(
                    Request::Observe(ObserveParams {
                        app: "com.apple.TextEdit".into(),
                        window: Some(window),
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
            if window == 2 {
                observation
                    .elements
                    .iter_mut()
                    .find(|e| e.id == 14)
                    .unwrap()
                    .secure = false;
            }
            service.record_observation(&observation);
        }
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit#secure")
        );
        invocation.effective.raw_arguments = Some(
            json!({"action":"type", "app":"TextEdit", "target":{"element":14}, "text":"secret"})
                .to_string(),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
    }

    #[test]
    fn outside_the_tui_the_call_is_refused() {
        let (service, mut invocation) =
            setup(FakeDesktop::with_textedit(), json!({"action":"list_apps"}));
        assert_eq!(
            prepare_invocation(&mut invocation, &RunConfig::default(), &service).unwrap_err(),
            "computer use is only available in the interactive TUI on macOS 14 or later; this session does not expose computer (main session only, no custom agent, and no whole-tool deny rule)"
        );
    }

    #[test]
    fn missing_permissions_are_named_with_where_to_turn_them_on() {
        for (accessibility, screen_recording, event_tap, missing) in [
            (true, false, true, "Screen Recording"),
            (false, false, false, "Accessibility and Screen Recording"),
            (true, true, false, "Accessibility"),
        ] {
            let mut desktop = FakeDesktop::with_textedit();
            desktop.permissions.accessibility = accessibility;
            desktop.permissions.screen_recording = screen_recording;
            desktop.permissions.event_tap = event_tap;
            let (service, mut invocation) =
                setup(desktop, json!({"action":"observe", "app":"TextEdit"}));
            assert_eq!(
                prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err(),
                format!(
                    "computer use needs {missing} for the app this terminal runs in (com.cmuxterm.app). Turn it on in System Settings > Privacy & Security; after granting Screen Recording, restart the terminal."
                )
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_and_execution_share_the_driver_installed_name_resolution() {
        use crate::extension::{ExtensionData, RuntimeExtensionStores};
        use orca_computer_use::driver::macos::test_resolve_app_from_roots;
        use orca_computer_use::inprocess::InProcessLauncher;
        use orca_computer_use::protocol::DriverError;
        use orca_computer_use::serve::{DriverHandler, LineWriter};
        use orca_core::cancel::CancelToken;
        use orca_core::tool_types::ToolStatus;
        use std::sync::Mutex;

        struct FileTreeDriver {
            roots: Vec<std::path::PathBuf>,
            desktop: FakeDesktop,
            requests: Arc<Mutex<Vec<Request>>>,
        }
        impl DriverHandler for FileTreeDriver {
            fn handle(
                &mut self,
                request: Request,
                events: &LineWriter,
            ) -> Result<Reply, DriverError> {
                self.requests.lock().unwrap().push(request.clone());
                match &request {
                    Request::ResolveApp {
                        app,
                        allow_not_running,
                    } => {
                        return Ok(Reply::Resolved {
                            app: test_resolve_app_from_roots(app, *allow_not_running, &self.roots)?,
                        });
                    }
                    Request::LaunchApp { app, .. } => {
                        // Launch uses the same native resolver with the approved ID.
                        let resolved = test_resolve_app_from_roots(app, true, &self.roots)?;
                        assert_eq!(&resolved.bundle_id, app);
                    }
                    _ => {}
                }
                self.desktop.handle(request, events)
            }
            fn shutdown(&mut self) {
                self.desktop.shutdown();
            }
        }
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let contents = temp.path().join("Applications/TeXtEdIt.app/Contents");
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.orca.test.approved.textedit</string>
<key>CFBundleName</key><string>TextEdit</string>
<key>CFBundleDisplayName</key><string>Localized Editor</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        let mut desktop =
            FakeDesktop::with_app("dev.orca.test.approved.textedit", "Localized Editor");
        desktop.quit_app("dev.orca.test.approved.textedit");
        let requests = Arc::new(Mutex::new(vec![]));
        let service =
            ComputerUseService::new_for_test(Arc::new(InProcessLauncher::new(FileTreeDriver {
                roots: vec![temp.path().join("Applications")],
                desktop,
                requests: requests.clone(),
            })));
        let (_, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"launch_app", "app":"textedit", "open":["/tmp/owned.txt"]}),
        );
        let config = interactive_config();
        prepare_invocation(&mut invocation, &config, &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("dev.orca.test.approved.textedit")
        );
        assert!(
            invocation
                .approval_preview
                .as_deref()
                .unwrap()
                .contains("Localized Editor (dev.orca.test.approved.textedit)")
        );
        assert!(requests.lock().unwrap().iter().any(|r| matches!(r, Request::ResolveApp { app, allow_not_running: true } if app == "textedit")));
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| matches!(r, Request::LaunchApp { .. })),
            "prepare must resolve without launching"
        );
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        thread_store.insert(service);
        let result = crate::computer_use::execute::execute(
            &invocation.effective,
            &config,
            0,
            RuntimeExtensionStores::new(&thread_store, &turn_store),
            temp.path(),
            &CancelToken::new(),
        );
        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        assert!(requests.lock().unwrap().iter().any(|r| matches!(r, Request::LaunchApp { app, open } if app == "dev.orca.test.approved.textedit" && open == &["/tmp/owned.txt"])));
    }

    #[test]
    fn a_missing_app_says_to_list_apps() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"observe", "app":"Missing"}),
        );
        assert_eq!(
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err(),
            "no app matches \"Missing\"; call list_apps"
        );
    }

    #[test]
    fn launch_app_resolves_an_app_that_is_not_running_without_launching_it() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.quit_app("com.apple.TextEdit");
        let (service, mut invocation) =
            setup(desktop, json!({"action":"launch_app", "app":"TextEdit"}));
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(
            invocation.effective.target.as_deref(),
            Some("com.apple.TextEdit")
        );
        let Reply::Apps { apps } = service
            .call(Request::ListApps, Duration::from_secs(30))
            .unwrap()
        else {
            panic!("apps")
        };
        assert!(apps.is_empty(), "authorization must not launch the app");
        invocation.effective.raw_arguments =
            Some(json!({"action":"observe", "app":"TextEdit"}).to_string());
        assert_eq!(
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err(),
            "no app matches \"TextEdit\"; call list_apps"
        );
    }

    fn check_computer_schema_fields(schema: &serde_json::Value) -> Result<(), String> {
        use orca_computer_use::call::action_fields_for_test;
        use std::collections::BTreeSet;
        let properties: BTreeSet<_> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut actual = BTreeSet::new();
        for action in schema["properties"]["action"]["enum"].as_array().unwrap() {
            let action = action.as_str().unwrap();
            let fields = action_fields_for_test(action)
                .ok_or_else(|| format!("schema action {action} has no parser allowlist"))?;
            for field in fields {
                actual.insert(*field);
            }
        }
        if actual != properties {
            return Err(format!(
                "computer action fields {actual:?} differ from tool schema properties {properties:?}"
            ));
        }
        Ok(())
    }

    #[test]
    fn computer_action_field_allowlists_match_the_live_tool_schema_properties() {
        let registry = orca_tools::registry::default_tool_registry();
        let schema = &registry.get("computer").unwrap().spec().input_schema;
        check_computer_schema_fields(schema).unwrap();
        // Future schema drift must fail this same contract, never quietly reject a new field.
        let mut changed = schema.clone();
        changed["properties"]["future_field"] = serde_json::json!({"type":"string"});
        let error = check_computer_schema_fields(&changed).unwrap_err();
        assert!(error.contains("future_field"));
        let mut changed_action = schema.clone();
        changed_action["properties"]["action"]["enum"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("future_action"));
        assert!(
            check_computer_schema_fields(&changed_action)
                .unwrap_err()
                .contains("future_action")
        );
    }

    #[test]
    fn a_non_modifier_key_is_rejected_before_driver_launch() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new_for_test(Arc::new(launcher.clone()));
        let (_, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"key", "app":"TextEdit", "keys":["t"], "modifiers":["s"]}),
        );
        let error =
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err();
        assert_eq!(error, "\"modifiers\" contains a non-modifier key \"s\"");
        assert_eq!(launcher.launches(), 0);
    }

    #[test]
    fn argument_errors_are_returned_before_launching_the_driver() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new_for_test(Arc::new(launcher.clone()));
        let (_, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"click", "app":"TextEdit"}),
        );
        assert_eq!(
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err(),
            "click needs \"target\": {\"element\": N} or {\"point\": [x, y]}"
        );
        assert_eq!(launcher.launches(), 0);
    }
    #[test]
    fn list_apps_checks_permissions_and_recovers_after_they_are_granted() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.permissions.accessibility = false;
        let launcher = FakeLauncher::new(desktop);
        let service = ComputerUseService::new_for_test(Arc::new(launcher.clone()));
        let (_, mut invocation) =
            setup(FakeDesktop::with_textedit(), json!({"action":"list_apps"}));
        let error =
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap_err();
        assert!(error.contains("Accessibility"), "{error}");
        assert!(
            error.contains("System Settings > Privacy & Security"),
            "{error}"
        );
        launcher.desktop(|desktop| desktop.permissions.accessibility = true);
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        assert_eq!(launcher.launches(), 2);
        assert_eq!(invocation.action, None);
    }

    #[test]
    fn launch_app_preview_includes_paths_and_urls() {
        let (service, mut invocation) = setup(
            FakeDesktop::with_textedit(),
            json!({"action":"launch_app", "app":"TextEdit", "open":["/tmp/report.txt", "https://example.test/private"]}),
        );
        prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
        let preview = invocation.approval_preview.unwrap();
        assert!(preview.contains("/tmp/report.txt"), "{preview}");
        assert!(
            preview.contains("https://example.test/private"),
            "{preview}"
        );
    }

    #[test]
    fn a_whole_tool_deny_refuses_list_apps_without_launching() {
        let launcher = FakeLauncher::new(FakeDesktop::with_textedit());
        let service = ComputerUseService::new_for_test(Arc::new(launcher.clone()));
        let mut config = interactive_config();
        config
            .permission_rules
            .rules
            .push(PermissionRule::whole_tool("computer", Decision::Deny));
        let (_, mut invocation) =
            setup(FakeDesktop::with_textedit(), json!({"action":"list_apps"}));
        assert!(prepare_invocation(&mut invocation, &config, &service).is_err());
        assert_eq!(launcher.launches(), 0);
    }

    #[test]
    fn separate_key_modifiers_preserve_password_approval() {
        for (key, expected) in [
            ("v", "com.apple.TextEdit#secure"),
            ("s", "com.apple.TextEdit"),
        ] {
            let (service, mut invocation) = setup(
                FakeDesktop::with_textedit(),
                json!({"action":"key", "app":"TextEdit", "keys":[key], "modifiers":["cmd"], "target":{"element":14}}),
            );
            let Reply::Observed { observation } = service
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
            service.record_observation(&observation);
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
            assert_eq!(
                invocation.effective.target.as_deref(),
                Some(expected),
                "{key}"
            );
        }
    }

    #[test]
    fn computer_modifier_schema_reaches_the_provider_and_parser() {
        let registry = orca_tools::registry::default_tool_registry();
        let spec = registry.get("computer").unwrap().spec();
        let wire = orca_provider::tool_schema::deepseek_tools_schema(&[
            orca_provider::tool_schema::ProviderToolDefinition {
                name: "computer".into(),
                description: spec.description.to_string(),
                input_schema: spec.input_schema.clone(),
                strict_capable: false,
            },
        ]);
        let modifiers =
            &wire[0]["function"]["parameters"]["properties"]["modifiers"]["items"]["enum"];
        assert_eq!(modifiers, &json!(["cmd", "shift", "option", "alt", "ctrl"]));
        for modifier in modifiers.as_array().unwrap() {
            for action in ["click", "key"] {
                let mut arguments = json!({
                    "action":action, "app":"TextEdit", "target":{"element":14},
                    "modifiers":[modifier],
                });
                if action == "key" {
                    arguments["keys"] = json!(["t"]);
                }
                orca_computer_use::call::ComputerCall::parse(&arguments).unwrap();
            }
        }
    }

    #[test]
    fn focused_password_fields_gate_text_keys_without_a_target() {
        for keys in [
            json!(["p"]),
            json!(["cmd", "v"]),
            json!(["shift", "option", "p"]),
            json!(["space"]),
            json!(["tab"]),
            json!(["ctrl", "p"]),
        ] {
            let (service, mut invocation) = setup(
                FakeDesktop::with_textedit(),
                json!({"action":"key", "app":"TextEdit", "keys":keys}),
            );
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
            observation.focused_secure = true;
            service.record_observation(&observation);
            prepare_invocation(&mut invocation, &interactive_config(), &service).unwrap();
            let secure = keys != json!(["tab"]) && keys != json!(["ctrl", "p"]);
            assert_eq!(
                invocation.effective.target.as_deref(),
                Some(if secure {
                    "com.apple.TextEdit#secure"
                } else {
                    "com.apple.TextEdit"
                }),
                "{keys}"
            );
        }
    }
}
