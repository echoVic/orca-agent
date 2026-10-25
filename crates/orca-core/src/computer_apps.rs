const TERMINALS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "com.mitchellh.ghostty",
    "dev.warp.Warp-Stable",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "com.github.wez.wezterm",
    "com.cmuxterm.app",
];
const IDES: &[&str] = &[
    "com.microsoft.VSCode",
    "com.todesktop.*",
    "com.jetbrains.*",
    "com.apple.dt.Xcode",
    "dev.zed.Zed",
];

/// Matches app bundle ids ignoring ASCII case, with a trailing `*` as a prefix.
pub fn listed(bundle_id: &str, patterns: &[&str]) -> bool {
    let folded = bundle_id.to_ascii_lowercase();
    patterns
        .iter()
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => folded.starts_with(&prefix.to_ascii_lowercase()),
            None => bundle_id.eq_ignore_ascii_case(pattern),
        })
}

/// Whether the app is a known terminal, ignoring ASCII case.
pub fn is_terminal(bundle_id: &str) -> bool {
    listed(bundle_id, TERMINALS)
}

/// Whether the app is a known terminal or IDE that can run commands.
pub fn runs_commands(bundle_id: &str) -> bool {
    is_terminal(bundle_id) || listed(bundle_id, IDES)
}

/// A computer approval target: the app, optionally one of the user's existing
/// windows, and whether the call types into a password field. Written as
/// `bundle`, `bundle#secure`, `bundle#window:<id>` or `bundle#window:<id>#secure`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputerTarget {
    pub app: String,
    pub window: Option<u32>,
    pub secure: bool,
}

impl ComputerTarget {
    /// A window part that is not a number stays in `app`.
    pub fn parse(target: &str) -> Self {
        let (rest, secure) = match target.strip_suffix("#secure") {
            Some(rest) => (rest, true),
            None => (target, false),
        };
        if let Some((app, window)) = rest.rsplit_once("#window:")
            && let Ok(window) = window.parse::<u32>()
        {
            return Self {
                app: app.to_owned(),
                window: Some(window),
                secure,
            };
        }
        Self {
            app: rest.to_owned(),
            window: None,
            secure,
        }
    }
}

impl std::fmt::Display for ComputerTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.app)?;
        if let Some(window) = self.window {
            write!(f, "#window:{window}")?;
        }
        if self.secure {
            f.write_str("#secure")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_are_recognized_without_case_sensitive_matching() {
        for app in TERMINALS {
            assert!(is_terminal(app), "{app}");
            assert!(is_terminal(&app.to_ascii_uppercase()), "{app}");
            assert!(runs_commands(app), "{app}");
        }
        for app in [
            "com.apple.Terminal.extra",
            "com.apple.TextEdit",
            "com.microsoft.VSCode",
        ] {
            assert!(!is_terminal(app), "{app}");
        }
    }

    #[test]
    fn ides_and_their_prefixes_are_recognized_without_case_sensitive_matching() {
        for app in [
            "com.microsoft.VSCode",
            "com.todesktop.cursor",
            "com.jetbrains.intellij",
            "com.apple.dt.Xcode",
            "dev.zed.Zed",
        ] {
            assert!(runs_commands(app), "{app}");
            assert!(runs_commands(&app.to_ascii_uppercase()), "{app}");
            assert!(!is_terminal(app), "{app}");
        }
        for app in [
            "com.apple.TextEdit",
            "com.jetbrainsish.app",
            "com.todesktopish.app",
            "com.jetbrains",
            "com.todesktop",
            "dev.zed.Zed.extra",
        ] {
            assert!(!runs_commands(app), "{app}");
        }
    }

    #[test]
    fn computer_targets_parse_and_print_back() {
        for (text, app, window, secure) in [
            ("com.apple.finder", "com.apple.finder", None, false),
            ("com.apple.finder#secure", "com.apple.finder", None, true),
            (
                "com.apple.finder#window:7",
                "com.apple.finder",
                Some(7),
                false,
            ),
            (
                "com.apple.finder#window:7#secure",
                "com.apple.finder",
                Some(7),
                true,
            ),
        ] {
            let target = ComputerTarget::parse(text);
            assert_eq!(
                target,
                ComputerTarget {
                    app: app.into(),
                    window,
                    secure
                },
                "{text}"
            );
            assert_eq!(target.to_string(), text);
        }
        // A window part that is not a number stays in the app name, so no
        // rule or option treats the target as a window.
        let odd = ComputerTarget::parse("com.apple.finder#window:x");
        assert_eq!(
            (odd.app.as_str(), odd.window),
            ("com.apple.finder#window:x", None)
        );
    }
}
