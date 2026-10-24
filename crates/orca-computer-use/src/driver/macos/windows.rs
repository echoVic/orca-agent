//! Which windows the model can use: the rule `list_apps`, `launch_app` and the
//! default-window choice share.
use crate::protocol::WindowInfo;

/// A layer-0 window as ScreenCaptureKit reports it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct CaptureWindowFacts {
    pub id: u32,
    pub title: String,
    pub frame: [f64; 4],
    pub on_screen: bool,
}

/// One entry of an app's `AXWindows`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AxWindowFacts {
    pub frame: [f64; 4],
    pub subrole: Option<String>,
    pub minimized: bool,
}

/// Document-sized: smaller unknown windows are status bubbles and popups.
const MIN_USABLE: (f64, f64) = (120.0, 60.0);

pub(super) fn usable(
    on_screen: bool,
    frame: [f64; 4],
    subrole: Option<&str>,
    minimized: bool,
) -> bool {
    minimized
        || (on_screen
            && (matches!(subrole, Some("AXStandardWindow" | "AXDialog"))
                || (frame[2] >= MIN_USABLE.0 && frame[3] >= MIN_USABLE.1)))
}

/// `ax` is `None` when the app did not answer accessibility in time; then only
/// visible document-sized windows are kept.
pub(super) fn usable_windows(
    capture: &[CaptureWindowFacts],
    ax: Option<&[AxWindowFacts]>,
) -> Vec<WindowInfo> {
    capture
        .iter()
        .filter_map(|window| {
            let (subrole, minimized) = match ax {
                Some(ax) => {
                    let facts = ax
                        .iter()
                        .find(|facts| super::frames_match(facts.frame, window.frame))?;
                    (facts.subrole.as_deref(), facts.minimized)
                }
                None => (None, false),
            };
            usable(window.on_screen, window.frame, subrole, minimized).then(|| WindowInfo {
                id: window.id,
                title: window.title.clone(),
                minimized,
            })
        })
        .collect()
}

/// One `AXWindows` entry, in order, for choosing the window observe/act use
/// when the model names none.
#[derive(Clone, Copy, Debug)]
pub(super) struct WindowChoice {
    pub is_main: bool,
    pub usable: bool,
    pub minimized: bool,
}

pub(super) fn default_window(choices: &[WindowChoice]) -> Option<usize> {
    choices
        .iter()
        .position(|choice| choice.is_main && choice.usable)
        .or_else(|| {
            choices
                .iter()
                .position(|choice| choice.usable && !choice.minimized)
        })
        .or_else(|| choices.iter().position(|choice| choice.usable))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(id: u32, frame: [f64; 4], on_screen: bool) -> CaptureWindowFacts {
        CaptureWindowFacts {
            id,
            title: format!("w{id}"),
            frame,
            on_screen,
        }
    }

    fn ax(frame: [f64; 4], subrole: &str, minimized: bool) -> AxWindowFacts {
        AxWindowFacts {
            frame,
            subrole: Some(subrole.into()),
            minimized,
        }
    }

    fn info(id: u32, minimized: bool) -> WindowInfo {
        WindowInfo {
            id,
            title: format!("w{id}"),
            minimized,
        }
    }

    #[test]
    fn usable_windows_keep_standard_large_and_minimized_windows() {
        let capture = vec![
            capture(1, [0.0, 0.0, 800.0, 600.0], true),
            // An off-screen helper with no AX window of its own.
            capture(2, [5000.0, 5000.0, 1.0, 1.0], false),
            // A status bubble: an AX window, but small and not standard.
            capture(3, [10.0, 1090.0, 138.0, 22.0], true),
            capture(4, [100.0, 100.0, 500.0, 400.0], false),
            capture(5, [900.0, 0.0, 300.0, 500.0], true),
        ];
        let ax = vec![
            ax([0.0, 0.0, 800.0, 600.0], "AXStandardWindow", false),
            ax([10.0, 1090.0, 138.0, 22.0], "AXUnknown", false),
            ax([100.0, 100.0, 500.0, 400.0], "AXStandardWindow", true),
            ax([900.0, 0.0, 300.0, 500.0], "AXFloatingWindow", false),
        ];
        assert_eq!(
            usable_windows(&capture, Some(&ax)),
            vec![info(1, false), info(4, true), info(5, false)]
        );
    }

    #[test]
    fn without_ax_answers_only_visible_document_sized_windows_remain() {
        let capture = vec![
            capture(1, [0.0, 0.0, 800.0, 600.0], true),
            capture(3, [10.0, 1090.0, 138.0, 22.0], true),
            capture(4, [100.0, 100.0, 500.0, 400.0], false),
        ];
        assert_eq!(usable_windows(&capture, None), vec![info(1, false)]);
    }

    #[test]
    fn a_small_dialog_is_usable_but_a_small_unknown_window_is_not() {
        assert!(usable(
            true,
            [0.0, 0.0, 100.0, 40.0],
            Some("AXDialog"),
            false
        ));
        assert!(!usable(
            true,
            [0.0, 0.0, 100.0, 40.0],
            Some("AXUnknown"),
            false
        ));
        assert!(!usable(
            false,
            [0.0, 0.0, 800.0, 600.0],
            Some("AXStandardWindow"),
            false
        ));
        assert!(usable(
            false,
            [0.0, 0.0, 800.0, 600.0],
            Some("AXStandardWindow"),
            true
        ));
    }

    #[test]
    fn the_default_window_prefers_a_usable_main_then_a_visible_one() {
        let choice = |is_main, usable, minimized| WindowChoice {
            is_main,
            usable,
            minimized,
        };
        assert_eq!(
            default_window(&[choice(false, true, false), choice(true, true, false)]),
            Some(1)
        );
        // A main window that is not usable (a bubble) loses to a usable one.
        assert_eq!(
            default_window(&[choice(true, false, false), choice(false, true, false)]),
            Some(1)
        );
        // Visible windows win over minimized ones.
        assert_eq!(
            default_window(&[choice(false, true, true), choice(false, true, false)]),
            Some(1)
        );
        assert_eq!(default_window(&[choice(false, true, true)]), Some(0));
        assert_eq!(default_window(&[choice(true, false, false)]), None);
    }
}
