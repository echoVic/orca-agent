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
}
