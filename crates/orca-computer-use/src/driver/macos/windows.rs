//! Which windows the model can use: the rule `list_apps`, `launch_app` and the
//! default-window choice share.
use crate::protocol::{DriverError, ErrorKind, WindowInfo};

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

/// Picks the AX window (and its capture window) observe and act use: an
/// explicit id must be a usable window of this app; without one, the usable
/// main window, else the first usable one, else (nothing usable) the main or
/// first window as before. Returns (AX index, capture index).
pub(super) fn choose_window(
    id: Option<u32>,
    pid: i32,
    app_name: &str,
    captures: &[super::capture::Window],
    ax: &[AxWindowFacts],
    main: Option<usize>,
) -> Result<(usize, usize), DriverError> {
    let capture_of = |facts: &AxWindowFacts| {
        captures
            .iter()
            .position(|c| c.pid == pid && super::frames_match(facts.frame, c.frame))
    };
    if let Some(id) = id {
        let not_usable = || {
            super::error(
                ErrorKind::WindowNotFound,
                crate::messages::not_usable_window(id, app_name),
            )
        };
        let capture = captures
            .iter()
            .position(|c| c.id == id && c.pid == pid)
            .ok_or_else(not_usable)?;
        let index = ax
            .iter()
            .position(|facts| super::frames_match(facts.frame, captures[capture].frame))
            .ok_or_else(not_usable)?;
        let (window, facts) = (&captures[capture], &ax[index]);
        if !usable(
            window.on_screen,
            window.frame,
            facts.subrole.as_deref(),
            facts.minimized,
        ) {
            return Err(not_usable());
        }
        return Ok((index, capture));
    }
    if ax.is_empty() {
        return Err(super::error(
            ErrorKind::WindowNotFound,
            crate::messages::no_window(app_name),
        ));
    }
    let choices: Vec<_> = ax
        .iter()
        .enumerate()
        .map(|(index, facts)| WindowChoice {
            is_main: main == Some(index),
            usable: capture_of(facts).is_some_and(|c| {
                usable(
                    captures[c].on_screen,
                    captures[c].frame,
                    facts.subrole.as_deref(),
                    facts.minimized,
                )
            }),
            minimized: facts.minimized,
        })
        .collect();
    let index = default_window(&choices).unwrap_or(main.unwrap_or(0));
    let capture = capture_of(&ax[index]).ok_or_else(|| {
        super::error(
            ErrorKind::WindowNotFound,
            "No capture window matches the accessibility window",
        )
    })?;
    Ok((index, capture))
}

/// Reads each candidate's facts while the deadline lasts. `read` gets the
/// seconds left and returns (frame, subrole, minimized). A candidate whose
/// frame cannot be read is dropped with its entry, so the lists stay aligned;
/// an unreadable subrole is absent and an unreadable minimized flag is false.
pub(super) fn gather_facts<W>(
    candidates: Vec<W>,
    deadline: std::time::Instant,
    mut read: impl FnMut(&W, f32) -> (Option<[f64; 4]>, Option<String>, Option<bool>),
) -> (Vec<W>, Vec<AxWindowFacts>) {
    let mut kept = Vec::with_capacity(candidates.len());
    let mut facts = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let Some(left) = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
        else {
            break;
        };
        let (frame, subrole, minimized) = read(&candidate, left.as_secs_f32());
        if let Some(frame) = frame {
            kept.push(candidate);
            facts.push(AxWindowFacts {
                frame,
                subrole,
                minimized: minimized.unwrap_or(false),
            });
        }
    }
    (kept, facts)
}

/// An app has a window when AX lists one or names a main window.
pub(super) fn has_window(ax_windows: usize, main_window: bool) -> bool {
    ax_windows > 0 || main_window
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

    fn window(id: u32, frame: [f64; 4], on_screen: bool) -> super::super::capture::Window {
        super::super::capture::Window {
            id,
            pid: 7,
            title: format!("w{id}"),
            frame,
            scale: 2.0,
            on_screen,
        }
    }

    const DOC: [f64; 4] = [0.0, 0.0, 800.0, 600.0];
    const BUBBLE: [f64; 4] = [10.0, 1090.0, 138.0, 22.0];
    const MINI: [f64; 4] = [100.0, 100.0, 500.0, 400.0];

    #[test]
    fn an_explicit_window_must_be_usable() {
        let captures = [
            window(1, DOC, true),
            window(3, BUBBLE, true),
            window(4, MINI, false),
        ];
        let facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(BUBBLE, "AXUnknown", false),
            ax(MINI, "AXStandardWindow", true),
        ];
        assert_eq!(
            choose_window(Some(1), 7, "App", &captures, &facts, None).unwrap(),
            (0, 0)
        );
        assert_eq!(
            choose_window(Some(4), 7, "App", &captures, &facts, None).unwrap(),
            (2, 2)
        );
        let bubble = choose_window(Some(3), 7, "App", &captures, &facts, None).unwrap_err();
        assert_eq!(bubble.message, crate::messages::not_usable_window(3, "App"));
        let missing = choose_window(Some(9), 7, "App", &captures, &facts, None).unwrap_err();
        assert_eq!(
            missing.message,
            crate::messages::not_usable_window(9, "App")
        );
    }

    #[test]
    fn the_default_window_skips_bubbles_and_falls_back_only_when_nothing_is_usable() {
        let captures = [window(3, BUBBLE, true), window(1, DOC, true)];
        let facts = [
            ax(BUBBLE, "AXUnknown", false),
            ax(DOC, "AXStandardWindow", false),
        ];
        // The bubble is AX's main window, but the document is usable.
        assert_eq!(
            choose_window(None, 7, "App", &captures, &facts, Some(0)).unwrap(),
            (1, 1)
        );
        // Only a small window: keep it usable as before rather than refuse the app.
        let only = [window(5, [0.0, 0.0, 100.0, 40.0], true)];
        let small = [ax([0.0, 0.0, 100.0, 40.0], "AXUnknown", false)];
        assert_eq!(
            choose_window(None, 7, "App", &only, &small, None).unwrap(),
            (0, 0)
        );
        let none = choose_window(None, 7, "App", &[], &[], None).unwrap_err();
        assert_eq!(none.message, crate::messages::no_window("App"));
    }

    #[test]
    fn unreadable_windows_are_skipped_and_the_lists_stay_aligned() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (kept, facts) = gather_facts(vec![1, 2, 3], deadline, |window, _| match window {
            // A helper whose frame cannot be read must not block the others.
            2 => (None, Some("AXStandardWindow".into()), Some(false)),
            // Unreadable subrole and minimized state read as absent and false.
            _ => (Some([0.0, 0.0, 800.0, 600.0]), None, None),
        });
        assert_eq!(kept, vec![1, 3]);
        assert_eq!(facts.len(), 2);
        assert!(
            facts
                .iter()
                .all(|facts| facts.subrole.is_none() && !facts.minimized)
        );
    }

    #[test]
    fn window_reads_stop_at_the_resolution_deadline() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(40);
        let (kept, _) = gather_facts(vec![1, 2, 3, 4], deadline, |_, timeout| {
            assert!(timeout > 0.0 && timeout <= 0.04);
            std::thread::sleep(std::time::Duration::from_millis(25));
            (Some([0.0, 0.0, 800.0, 600.0]), None, Some(false))
        });
        assert_eq!(kept, vec![1, 2]);
    }

    #[test]
    fn a_main_window_alone_counts_as_a_window() {
        assert!(has_window(0, true));
        assert!(has_window(1, false));
        assert!(!has_window(0, false));
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
