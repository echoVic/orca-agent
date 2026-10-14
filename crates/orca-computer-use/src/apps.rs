use orca_core::computer_apps::{is_terminal, listed, runs_commands};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RiskTier {
    HostTerminal,
    Forbidden,
    Warning(&'static str),
    Normal,
}

const PASSWORD_MANAGERS: &[&str] = &[
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.1password.*",
    "com.agilebits.*",
    "com.bitwarden.*",
];
const BROWSERS: &[&str] = &[
    "com.apple.Safari",
    "com.google.Chrome",
    "org.mozilla.firefox",
    "company.thebrowser.Browser",
    "com.microsoft.edgemac",
    "com.brave.Browser",
];

pub fn risk_tier(
    bundle_id: &str,
    category: Option<&str>,
    host_bundle_id: Option<&str>,
) -> RiskTier {
    let terminal = is_terminal(bundle_id);
    if host_bundle_id.is_some_and(|host| host.eq_ignore_ascii_case(bundle_id))
        || (host_bundle_id.is_none() && terminal)
    {
        return RiskTier::HostTerminal;
    }
    if listed(bundle_id, PASSWORD_MANAGERS) || category == Some("public.app-category.finance") {
        return RiskTier::Forbidden;
    }
    if runs_commands(bundle_id) {
        return RiskTier::Warning("can run any command, outside Orca's sandbox");
    }
    match bundle_id.to_ascii_lowercase().as_str() {
        "com.apple.finder" => RiskTier::Warning("can read and write any file"),
        "com.apple.systempreferences" => RiskTier::Warning("changes system settings"),
        _ if listed(bundle_id, BROWSERS) => {
            RiskTier::Warning("web pages may carry instructions aimed at the agent")
        }
        _ => RiskTier::Normal,
    }
}

pub fn host_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_foundation::NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            >= 14
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part5_risk_case_variants_cannot_lower_forbidden_tiers() {
        for id in [
            "COM.APPLE.PASSWORDS",
            "com.apple.passwords",
            "COM.1PASSWORD.AGENT",
            "COM.BITWARDEN.DESKTOP",
            "COM.APPLE.KEYCHAINACCESS",
        ] {
            assert_eq!(
                risk_tier(id, None, Some("dev.orca.host")),
                RiskTier::Forbidden,
                "{id}"
            );
        }
        assert_eq!(risk_tier("dev.orca.ordinary", None, None), RiskTier::Normal);
    }

    #[test]
    fn part5_risk_case_variants_keep_warning_tiers() {
        for id in [
            "COM.APPLE.FINDER",
            "COM.APPLE.SYSTEMPREFERENCES",
            "COM.MICROSOFT.VSCODE",
            "COM.GOOGLE.CHROME",
            "COM.JETBRAINS.IDEA",
        ] {
            assert!(
                matches!(
                    risk_tier(id, None, Some("dev.orca.host")),
                    RiskTier::Warning(_)
                ),
                "{id}"
            );
        }
    }

    #[test]
    fn part5_risk_host_identity_and_terminal_fallback_ignore_case() {
        assert_eq!(
            risk_tier("DEV.ORCA.HOST", None, Some("dev.orca.host")),
            RiskTier::HostTerminal
        );
        assert_eq!(
            risk_tier("COM.APPLE.TERMINAL", None, None),
            RiskTier::HostTerminal
        );
    }

    #[test]
    fn the_host_terminal_and_password_managers_are_tiered() {
        assert_eq!(
            risk_tier("com.cmuxterm.app", None, Some("com.cmuxterm.app")),
            RiskTier::HostTerminal
        );
        assert_eq!(
            risk_tier("com.1password.1password", None, Some("com.cmuxterm.app")),
            RiskTier::Forbidden
        );
        assert_eq!(
            risk_tier(
                "com.example.bank",
                Some("public.app-category.finance"),
                None
            ),
            RiskTier::Forbidden
        );
        assert!(matches!(
            risk_tier("com.apple.Safari", None, Some("com.cmuxterm.app")),
            RiskTier::Warning(_)
        ));
        assert_eq!(
            risk_tier("com.mitchellh.ghostty", None, None),
            RiskTier::HostTerminal
        );
        assert_eq!(
            risk_tier("com.apple.TextEdit", None, Some("com.cmuxterm.app")),
            RiskTier::Normal
        );
    }

    #[test]
    fn all_listed_apps_and_prefixes_have_the_exact_warning() {
        for id in [
            "com.apple.Terminal",
            "com.googlecode.iterm2",
            "com.mitchellh.ghostty",
            "dev.warp.Warp-Stable",
            "net.kovidgoyal.kitty",
            "org.alacritty",
            "com.github.wez.wezterm",
            "com.cmuxterm.app",
        ] {
            assert_eq!(risk_tier(id, None, None), RiskTier::HostTerminal, "{id}");
            assert_eq!(
                risk_tier(id, None, Some(id)),
                RiskTier::HostTerminal,
                "{id}"
            );
            assert_eq!(
                risk_tier(id, None, Some("other")),
                RiskTier::Warning("can run any command, outside Orca's sandbox"),
                "{id}"
            );
        }
        for id in [
            "com.microsoft.VSCode",
            "com.todesktop.cursor",
            "com.jetbrains.idea",
            "com.apple.dt.Xcode",
            "dev.zed.Zed",
        ] {
            assert_eq!(
                risk_tier(id, None, None),
                RiskTier::Warning("can run any command, outside Orca's sandbox"),
                "{id}"
            );
        }
        for id in [
            "com.apple.Safari",
            "com.google.Chrome",
            "org.mozilla.firefox",
            "company.thebrowser.Browser",
            "com.microsoft.edgemac",
            "com.brave.Browser",
        ] {
            assert_eq!(
                risk_tier(id, None, None),
                RiskTier::Warning("web pages may carry instructions aimed at the agent"),
                "{id}"
            );
        }
        assert_eq!(
            risk_tier("com.apple.finder", None, None),
            RiskTier::Warning("can read and write any file")
        );
        assert_eq!(
            risk_tier("com.apple.systempreferences", None, None),
            RiskTier::Warning("changes system settings")
        );
    }

    #[test]
    fn forbidden_apps_and_host_identity_take_precedence_over_warnings() {
        for id in [
            "com.apple.keychainaccess",
            "com.apple.Passwords",
            "com.1password.example",
            "com.agilebits.onepassword",
            "com.bitwarden.desktop",
        ] {
            assert_eq!(risk_tier(id, None, None), RiskTier::Forbidden, "{id}");
        }
        assert_eq!(
            risk_tier(
                "com.apple.Safari",
                Some("public.app-category.finance"),
                None
            ),
            RiskTier::Forbidden
        );
        assert_eq!(
            risk_tier("com.example.host", None, Some("com.example.host")),
            RiskTier::HostTerminal
        );
        for id in [
            "com.1passwordish.app",
            "com.jetbrainsish.app",
            "com.todesktopish.app",
            "com.apple.Terminal.extra",
        ] {
            assert_eq!(risk_tier(id, None, None), RiskTier::Normal, "{id}");
        }
    }

    #[test]
    fn host_support_matches_the_platform_version() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            host_supported(),
            objc2_foundation::NSProcessInfo::processInfo()
                .operatingSystemVersion()
                .majorVersion
                >= 14
        );
        #[cfg(not(target_os = "macos"))]
        assert!(!host_supported());
    }
}
