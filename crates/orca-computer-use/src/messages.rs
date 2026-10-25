//! Window errors shared by the macOS driver and the fake desktop, each with
//! the step the model should take next.

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

#[cfg(test)]
mod tests {
    use super::*;

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
