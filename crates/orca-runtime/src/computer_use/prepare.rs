use std::time::Duration;

use orca_computer_use::apps::{RiskTier, risk_tier};
use orca_computer_use::call::ComputerCall;
use orca_computer_use::client::CallError;
use orca_computer_use::protocol::{Action, ErrorKind, Reply, Request, Target};
use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
use orca_core::config::RunConfig;

use super::ComputerUseService;
use crate::tool_invocation::ToolInvocation;

/// Resolve the app and apply authorization restrictions before asking approval.
pub(crate) fn prepare_invocation(
    invocation: &mut ToolInvocation,
    config: &RunConfig,
    service: &ComputerUseService,
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
            "{} into a password field in {}",
            call.action_name(),
            app.name
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
    #[cfg(test)]
    let host_supported = service.map_or(host_supported, ComputerUseService::test_host_supported);
    #[cfg(not(test))]
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
        for action in ["click", "type", "key", "scroll", "launch_app"] {
            let mut desktop = FakeDesktop::with_textedit();
            desktop.permissions.accessibility = false;
            let (service, mut invocation) = setup(
                desktop,
                json!({"action":action, "app":"TextEdit", "target":{"element":1}, "text":"test", "keys":["a"], "direction":"up"}),
            );
            assert_eq!(
                prepare_invocation(&mut invocation, &config, &service).unwrap_err(),
                "plan mode only allows list_apps and observe"
            );
        }
        for action in ["observe", "list_apps"] {
            let (service, mut invocation) = setup(
                FakeDesktop::with_textedit(),
                json!({"action":action, "app":"TextEdit"}),
            );
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
            Some("type into a password field in TextEdit")
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
