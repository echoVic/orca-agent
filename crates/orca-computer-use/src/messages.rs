//! Window errors shared by the macOS driver and the fake desktop, each with
//! the step the model should take next.

pub const OPENED_IN_USER_WINDOW: &str = "The file or URL may have opened inside one of the user's windows (for example as a tab); Orca did not look at it. Open a new window first to work with it.";

pub const CMD_N_OPENED_NO_WINDOW: &str = "Cmd+N opened no new window; the app may have acted in the user's window instead. Observe again before going on.";

pub const NO_WINDOW_NEXT_STEP: &str = "For an app-level shortcut such as cmd+n, use key without window; to open a file or URL, use launch_app with open.";

/// A note, not an error: the action already happened, so it must not be retried.
pub const WINDOW_UNUSABLE_AFTER_ACTION: &str =
    "The action was delivered, but its window is no longer usable; observe again.";

pub fn not_usable_window(id: u32, app: &str) -> String {
    format!(
        "Window {id} is not a window of {app} that Orca can use. Call list_apps for its usable windows, or omit window to use the main window."
    )
}

pub fn not_observed(id: u32) -> String {
    format!("Window {id} has not been observed yet. Call observe on it first.")
}

pub fn no_window(app: &str) -> String {
    format!("{app} has no window yet. {NO_WINDOW_NEXT_STEP}")
}

pub fn app_wide_key(app: &str) -> String {
    format!(
        "That shortcut acts on every window of {app}, including the user's own, so Orca does not send it while they are open. Ask the user instead."
    )
}

pub fn window_kept(title: &str) -> String {
    format!(
        "The user kept their window {title:?} to themselves. Leave it alone: open a new window instead (for example with key cmd+n, or launch_app with open), or ask the user."
    )
}

pub fn user_window(id: u32, title: &str) -> String {
    format!(
        "Window {id} (\"{title}\") is the user's own window. Call again with window {id} so Orca can ask the user, or open a new window."
    )
}

pub fn user_window_only(app: &str, windows: &[(u32, String)]) -> String {
    let listed = windows
        .iter()
        .map(|(id, title)| format!("{id} \"{title}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{app} only has the user's own windows: {listed}. To use one, call again with its window number so Orca can ask the user; or open a new window (for example with key cmd+n, or launch_app with open)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_kept_names_the_window_and_the_next_step() {
        assert_eq!(
            window_kept("probe.rtf"),
            "The user kept their window \"probe.rtf\" to themselves. Leave it alone: open a new window instead (for example with key cmd+n, or launch_app with open), or ask the user."
        );
    }

    #[test]
    fn opened_in_user_window_note_is_exact() {
        assert_eq!(
            OPENED_IN_USER_WINDOW,
            "The file or URL may have opened inside one of the user's windows (for example as a tab); Orca did not look at it. Open a new window first to work with it."
        );
    }

    #[test]
    fn app_wide_shortcut_message_names_the_app_and_user_boundary() {
        assert_eq!(
            app_wide_key("TextEdit"),
            "That shortcut acts on every window of TextEdit, including the user's own, so Orca does not send it while they are open. Ask the user instead."
        );
    }

    #[test]
    fn cmd_n_without_a_new_window_warns_of_possible_user_window_action() {
        assert_eq!(
            CMD_N_OPENED_NO_WINDOW,
            "Cmd+N opened no new window; the app may have acted in the user's window instead. Observe again before going on."
        );
    }

    #[test]
    fn user_window_messages_name_the_windows_and_both_ways_on() {
        assert_eq!(
            user_window(24538, "files120"),
            "Window 24538 (\"files120\") is the user's own window. Call again with window 24538 so Orca can ask the user, or open a new window."
        );
        assert_eq!(
            user_window_only(
                "Finder",
                &[(24538, "files120".into()), (21631, "out".into())]
            ),
            "Finder only has the user's own windows: 24538 \"files120\", 21631 \"out\". To use one, call again with its window number so Orca can ask the user; or open a new window (for example with key cmd+n, or launch_app with open)."
        );
    }

    #[test]
    fn messages_name_the_window_and_the_next_step() {
        assert_eq!(
            not_usable_window(48008, "Finder"),
            "Window 48008 is not a window of Finder that Orca can use. Call list_apps for its usable windows, or omit window to use the main window."
        );
        assert_eq!(
            not_observed(47795),
            "Window 47795 has not been observed yet. Call observe on it first."
        );
        assert_eq!(
            no_window("TextEdit"),
            "TextEdit has no window yet. For an app-level shortcut such as cmd+n, use key without window; to open a file or URL, use launch_app with open."
        );
    }
}
