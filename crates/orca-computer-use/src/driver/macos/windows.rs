//! Which windows the model can use: the rule `list_apps`, `launch_app` and the
//! default-window choice share.
use crate::protocol::{Action, DriverError, ErrorKind, WindowInfo};

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
    pub id: Option<u32>,
    pub frame: [f64; 4],
    pub subrole: Option<String>,
    pub minimized: bool,
}

impl AxWindowFacts {
    fn matches(&self, id: u32, frame: [f64; 4]) -> bool {
        match self.id {
            Some(ax_id) => ax_id == id,
            None => super::frames_match(self.frame, frame),
        }
    }
}

// Prefer a known identity over an AX entry which can only match by frame.
fn ax_index(ax: &[AxWindowFacts], id: u32, frame: [f64; 4]) -> Option<usize> {
    ax.iter()
        .position(|facts| facts.id == Some(id))
        .or_else(|| {
            ax.iter()
                .position(|facts| facts.id.is_none() && facts.matches(id, frame))
        })
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
                    let facts = &ax[ax_index(ax, window.id, window.frame)?];
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

/// Classifies the app-shortcut path when AX reports no window. Captured user
/// windows still require lone Cmd+N; captures of Orca's own keep the old path.
/// Returns whether the caller should use its existing windowless validation.
pub(super) fn app_shortcut_windowless(
    pid: i32,
    app_name: &str,
    captures: &[super::capture::Window],
    user_windows: &[u32],
    action: &Action,
) -> Result<bool, DriverError> {
    let theirs: Vec<_> = captures
        .iter()
        .filter(|capture| {
            capture.layer == 0 && capture.pid == pid && user_windows.contains(&capture.id)
        })
        .map(|capture| (capture.id, capture.title.clone()))
        .collect();
    if theirs.is_empty() {
        return Ok(true);
    }
    if matches!(action, Action::Key { keys, target: None }
        if crate::keys::parse_combo(keys).is_ok_and(|stroke| stroke.opens_new_window()))
    {
        return Ok(false);
    }
    Err(super::error(
        ErrorKind::UserWindow,
        crate::messages::user_window_only(app_name, &theirs),
    ))
}

/// Picks the AX window (and its capture window) observe and act use: an
/// explicit id must be a usable layer-0 window of this app; without one, the usable
/// main window, else the first usable one, else (nothing usable) the main or
/// first window as before. Returns (AX index, capture index).
/// It never uses one of `user_windows`: a named one is refused, and when
/// nothing else is usable the user's windows are listed in the error; only
/// an app with none of them falls back to its main or first window.
pub(super) fn choose_window(
    id: Option<u32>,
    pid: i32,
    app_name: &str,
    captures: &[super::capture::Window],
    ax: &[AxWindowFacts],
    main: Option<usize>,
    user_windows: &[u32],
) -> Result<(usize, usize), DriverError> {
    let no_capture = || {
        super::error(
            ErrorKind::WindowNotFound,
            "No capture window matches the accessibility window",
        )
    };
    let capture_of = |facts: &AxWindowFacts| {
        captures
            .iter()
            .position(|c| c.layer == 0 && c.pid == pid && facts.matches(c.id, c.frame))
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
            .position(|c| c.layer == 0 && c.id == id && c.pid == pid)
            .ok_or_else(not_usable)?;
        if user_windows.contains(&id) {
            return Err(super::error(
                ErrorKind::UserWindow,
                crate::messages::user_window(id, &captures[capture].title),
            ));
        }
        let index = ax_index(ax, id, captures[capture].frame).ok_or_else(not_usable)?;
        if let Some(user) = captures.iter().find(|capture| {
            ax[index].id.is_none()
                && capture.layer == 0
                && capture.pid == pid
                && user_windows.contains(&capture.id)
                && ax[index].matches(capture.id, capture.frame)
        }) {
            return Err(super::error(
                ErrorKind::UserWindow,
                crate::messages::user_window(user.id, &user.title),
            ));
        }
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
    let theirs = |facts: &AxWindowFacts| {
        // Without an AX id, any same-frame user capture makes this window
        // unsafe, even if Orca's capture appears first.
        captures.iter().any(|capture| {
            capture.layer == 0
                && capture.pid == pid
                && user_windows.contains(&capture.id)
                && facts.matches(capture.id, capture.frame)
        })
    };
    let choices: Vec<_> = ax
        .iter()
        .enumerate()
        .map(|(index, facts)| WindowChoice {
            is_main: main == Some(index),
            // The user's windows are never a default choice.
            usable: !theirs(facts)
                && capture_of(facts).is_some_and(|c| {
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
    let index = match default_window(&choices) {
        Some(index) => index,
        None => {
            let mut listed_ids = std::collections::HashSet::new();
            // AX may report only the desktop, while CaptureKit still sees
            // the user's document. Preserve the refusal and lone Cmd+N path.
            let listed: Vec<(u32, String)> = captures
                .iter()
                .filter(|capture| {
                    capture.layer == 0 && capture.pid == pid && user_windows.contains(&capture.id)
                })
                .filter(|capture| listed_ids.insert(capture.id))
                .map(|capture| (capture.id, capture.title.clone()))
                .collect();
            if !listed.is_empty() {
                return Err(super::error(
                    ErrorKind::UserWindow,
                    crate::messages::user_window_only(app_name, &listed),
                ));
            }
            // AX can still name the desktop as main. The old main/first
            // fallback applies only to candidates with a layer-0 capture.
            main.filter(|&index| capture_of(&ax[index]).is_some())
                .or_else(|| ax.iter().position(|facts| capture_of(facts).is_some()))
                .ok_or_else(no_capture)?
        }
    };
    let capture = capture_of(&ax[index]).ok_or_else(no_capture)?;
    Ok((index, capture))
}

/// Reads each candidate's facts while the deadline lasts. `read` gets the
/// seconds left and returns its facts. A candidate whose frame cannot be read
/// is dropped with its entry, so the lists stay aligned;
/// an unreadable subrole is absent and an unreadable minimized flag is false.
pub(super) fn gather_facts<W>(
    candidates: Vec<W>,
    deadline: std::time::Instant,
    mut read: impl FnMut(&W, f32) -> Option<AxWindowFacts>,
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
        if let Some(window_facts) = read(&candidate, left.as_secs_f32()) {
            kept.push(candidate);
            facts.push(window_facts);
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

    fn key(keys: &[&str]) -> crate::protocol::Action {
        crate::protocol::Action::Key {
            keys: keys.iter().map(|key| key.to_string()).collect(),
            target: None,
        }
    }

    #[test]
    fn ax_windowless_shortcuts_only_allow_cmd_n_when_user_captures_remain() {
        let captures = [window(1, DOC, false)];
        assert!(
            !app_shortcut_windowless(7, "Finder", &captures, &[1], &key(&["cmd", "n"])).unwrap()
        );
        for keys in [
            &["cmd", "w"][..],
            &["cmd", "q"],
            &["ctrl", "n"],
            &["cmd", "shift", "n"],
        ] {
            let refused =
                app_shortcut_windowless(7, "Finder", &captures, &[1], &key(keys)).unwrap_err();
            assert_eq!(refused.kind, ErrorKind::UserWindow);
            assert_eq!(
                refused.message,
                crate::messages::user_window_only("Finder", &[(1, "w1".into())])
            );
        }
        let targeted = crate::protocol::Action::Key {
            keys: vec!["cmd".into(), "n".into()],
            target: Some(crate::protocol::Target::Element { element: 1 }),
        };
        assert_eq!(
            app_shortcut_windowless(7, "Finder", &captures, &[1], &targeted)
                .unwrap_err()
                .kind,
            ErrorKind::UserWindow
        );
    }

    #[test]
    fn ax_windowless_without_user_captures_keeps_the_existing_shortcut_path() {
        for keys in [&["cmd", "w"][..], &["cmd", "q"], &["ctrl", "n"]] {
            assert!(app_shortcut_windowless(7, "Finder", &[], &[1], &key(keys)).unwrap());
            // A capture of Orca's own that AX missed does not change the old path.
            assert!(
                app_shortcut_windowless(7, "Finder", &[window(2, DOC, false)], &[1], &key(keys))
                    .unwrap()
            );
            // Another process's user window cannot restrict this app's shortcuts.
            assert!(
                app_shortcut_windowless(8, "Other", &[window(1, DOC, false)], &[1], &key(keys))
                    .unwrap()
            );
        }
    }

    fn desktop() -> super::super::capture::Window {
        let mut capture = window(45, [0.0, 0.0, 1920.0, 1080.0], true);
        capture.layer = -1;
        capture
    }

    fn ax_for_captures(
        captures: &[super::super::capture::Window],
        with_ids: bool,
    ) -> Vec<AxWindowFacts> {
        captures
            .iter()
            .map(|capture| AxWindowFacts {
                id: with_ids.then_some(capture.id),
                ..ax(capture.frame, "AXStandardWindow", false)
            })
            .collect()
    }

    #[test]
    fn non_document_windows_do_not_replace_excluded_user_windows() {
        // Only the desktop is usable: the user's document is off screen.
        let captures = [desktop(), window(1, DOC, false)];
        for with_ids in [true, false] {
            let facts = ax_for_captures(&captures, with_ids);
            let refused = choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[1])
                .expect_err("the desktop must not be a default window");
            assert_eq!(refused.kind, ErrorKind::UserWindow);
            assert_eq!(
                refused.message,
                crate::messages::user_window_only("Finder", &[(1, "w1".into())])
            );
        }
    }

    #[test]
    fn non_document_windows_do_not_hide_ax_invisible_user_documents() {
        let captures = [desktop(), window(1, DOC, false)];
        for with_ids in [true, false] {
            // AX sees only the desktop; CaptureKit still sees the user's window.
            let facts = ax_for_captures(&captures[..1], with_ids);
            let refused = choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[1])
                .expect_err("an AX-invisible user document still requires lone Cmd+N");
            assert_eq!(refused.kind, ErrorKind::UserWindow);
            assert_eq!(
                refused.message,
                crate::messages::user_window_only("Finder", &[(1, "w1".into())])
            );
        }
    }

    #[test]
    fn explicit_non_document_windows_are_not_usable_even_if_excluded() {
        for layer in [-1, 1] {
            let mut capture = desktop();
            capture.layer = layer;
            let captures = [capture, window(1, DOC, false)];
            for with_ids in [true, false] {
                let facts = ax_for_captures(&captures, with_ids);
                for excluded in [&[1][..], &[1, 45][..]] {
                    let refused =
                        choose_window(Some(45), 7, "Finder", &captures, &facts, Some(0), excluded)
                            .expect_err("naming the desktop cannot make it usable");
                    assert_eq!(refused.kind, ErrorKind::WindowNotFound);
                    assert_eq!(
                        refused.message,
                        crate::messages::not_usable_window(45, "Finder")
                    );
                }
            }
        }
    }

    #[test]
    fn a_layer_zero_owned_window_wins_over_a_non_document_main_window() {
        let captures = [desktop(), window(1, DOC, false), window(2, MINI, true)];
        for with_ids in [true, false] {
            let facts = ax_for_captures(&captures, with_ids);
            assert_eq!(
                choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[1]).unwrap(),
                (2, 2)
            );
            assert_eq!(
                choose_window(Some(2), 7, "Finder", &captures, &facts, Some(0), &[1]).unwrap(),
                (2, 2)
            );
        }
    }

    #[test]
    fn non_document_windows_are_never_a_main_window_fallback_or_frame_match() {
        let captures = [desktop()];
        for with_ids in [true, false] {
            let facts = ax_for_captures(&captures, with_ids);
            assert_eq!(
                choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[])
                    .expect_err("the fallback must not use the desktop")
                    .kind,
                ErrorKind::WindowNotFound
            );
        }
        // An id-less AX window must match the layer-0 capture even when a
        // non-document capture with exactly the same frame precedes it.
        let captures = [desktop(), window(2, captures[0].frame, true)];
        let facts = ax_for_captures(&captures[1..], false);
        assert_eq!(
            choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[45]).unwrap(),
            (0, 1)
        );
        let captures = [desktop(), window(2, BUBBLE, true)];
        for with_ids in [true, false] {
            let mut facts = ax_for_captures(&captures, with_ids);
            facts[1].subrole = Some("AXUnknown".into());
            assert_eq!(
                choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[]).unwrap(),
                (1, 1)
            );
        }
    }

    #[test]
    fn ax_windowless_shortcuts_ignore_non_document_captures() {
        let captures = [desktop(), window(1, DOC, false)];
        // Even an excluded non-document id must not create a user-document boundary.
        assert!(
            app_shortcut_windowless(7, "Finder", &captures[..1], &[45], &key(&["cmd", "w"]))
                .unwrap()
        );
        let refused =
            app_shortcut_windowless(7, "Finder", &captures, &[45, 1], &key(&["cmd", "w"]))
                .unwrap_err();
        assert_eq!(
            refused.message,
            crate::messages::user_window_only("Finder", &[(1, "w1".into())])
        );
        assert!(
            !app_shortcut_windowless(7, "Finder", &captures, &[45, 1], &key(&["cmd", "n"]))
                .unwrap()
        );
    }

    #[test]
    fn same_frame_windows_with_ids_choose_orcas_and_refuse_the_users() {
        let captures = [window(2, DOC, true), window(1, DOC, true)];
        let mut facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(DOC, "AXStandardWindow", false),
        ];
        // AX order differs from capture order and the user's window is main.
        facts[0].id = Some(1);
        facts[1].id = Some(2);
        assert_eq!(
            choose_window(Some(2), 7, "Chrome", &captures, &facts, Some(0), &[1]).unwrap(),
            (1, 0)
        );
        assert_eq!(
            choose_window(None, 7, "Chrome", &captures, &facts, Some(0), &[1]).unwrap(),
            (1, 0)
        );
        assert_eq!(
            choose_window(Some(1), 7, "Chrome", &captures, &facts, Some(0), &[1])
                .unwrap_err()
                .kind,
            ErrorKind::UserWindow
        );
    }

    #[test]
    fn usable_windows_match_ids_instead_of_same_frames() {
        let captures = [capture(2, DOC, false), capture(1, DOC, false)];
        let mut facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(DOC, "AXStandardWindow", true),
        ];
        facts[0].id = Some(1);
        facts[1].id = Some(2);
        assert_eq!(usable_windows(&captures, Some(&facts)), vec![info(2, true)]);
        // A known but different id must never fall back to the same frame.
        assert!(usable_windows(&captures[..1], Some(&facts[..1])).is_empty());
        assert!(
            choose_window(
                Some(2),
                7,
                "Chrome",
                &[window(2, DOC, true)],
                &facts[..1],
                None,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn explicit_window_refuses_a_same_frame_user_capture() {
        let captures = [window(2, DOC, true), window(1, DOC, true)];
        let facts = [ax(DOC, "AXStandardWindow", false)];
        let refused =
            choose_window(Some(2), 7, "Finder", &captures, &facts, None, &[1]).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::UserWindow);
        assert_eq!(refused.message, crate::messages::user_window(1, "w1"));
    }

    #[test]
    fn explicit_window_without_same_frame_user_capture_is_chosen() {
        let captures = [window(2, DOC, true), window(1, MINI, true)];
        let facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(MINI, "AXStandardWindow", false),
        ];
        assert_eq!(
            choose_window(Some(2), 7, "Finder", &captures, &facts, None, &[1]).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn explicit_approved_user_window_works_with_another_user_window_excluded() {
        let captures = [window(1, DOC, true), window(2, MINI, true)];
        let facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(MINI, "AXStandardWindow", false),
        ];
        assert_eq!(
            choose_window(Some(1), 7, "Finder", &captures, &facts, None, &[2]).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn same_frame_captures_fail_closed_and_list_the_user_id_once() {
        // Orca's capture comes first: the first frame match must not hide the user's.
        let captures = [window(2, DOC, true), window(1, DOC, true)];
        let facts = [ax(DOC, "AXStandardWindow", false)];
        let refused =
            choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[1]).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::UserWindow);
        assert_eq!(
            refused.message,
            crate::messages::user_window_only("Finder", &[(1, "w1".into())])
        );
        // Multiple AX entries with that frame must still list each user id once.
        let repeated = [
            ax(DOC, "AXStandardWindow", false),
            ax(DOC, "AXStandardWindow", false),
        ];
        let refused =
            choose_window(None, 7, "Finder", &captures, &repeated, Some(0), &[1]).unwrap_err();
        assert_eq!(
            refused.message,
            crate::messages::user_window_only("Finder", &[(1, "w1".into())])
        );
    }

    #[test]
    fn the_default_window_never_falls_back_to_a_user_window() {
        let captures = [window(1, DOC, true), window(2, MINI, true)];
        let facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(MINI, "AXStandardWindow", false),
        ];
        // Window 1 is the user's and AX's main window: the default skips it.
        assert_eq!(
            choose_window(None, 7, "Finder", &captures, &facts, Some(0), &[1]).unwrap(),
            (1, 1)
        );
        // Only the user's window is left: refuse and name it.
        let only = choose_window(
            None,
            7,
            "Finder",
            &captures[..1],
            &facts[..1],
            Some(0),
            &[1],
        )
        .unwrap_err();
        assert_eq!(only.kind, ErrorKind::UserWindow);
        assert_eq!(
            only.message,
            crate::messages::user_window_only("Finder", &[(1, "w1".into())])
        );
        // Nothing usable but a bubble of the app's own: the old fallback to
        // the main window would land on the user's window, so refuse instead.
        let bubbles = [window(1, DOC, true), window(3, BUBBLE, true)];
        let bubble_facts = [
            ax(DOC, "AXStandardWindow", false),
            ax(BUBBLE, "AXUnknown", false),
        ];
        let refused =
            choose_window(None, 7, "Finder", &bubbles, &bubble_facts, Some(0), &[1]).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::UserWindow);
        // Naming it is refused here; the runtime asks the user first.
        let named = choose_window(Some(1), 7, "Finder", &captures, &facts, None, &[1]).unwrap_err();
        assert_eq!(named.kind, ErrorKind::UserWindow);
        assert_eq!(named.message, crate::messages::user_window(1, "w1"));
    }

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
            id: None,
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
            layer: 0,
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
            choose_window(Some(1), 7, "App", &captures, &facts, None, &[]).unwrap(),
            (0, 0)
        );
        assert_eq!(
            choose_window(Some(4), 7, "App", &captures, &facts, None, &[]).unwrap(),
            (2, 2)
        );
        let bubble = choose_window(Some(3), 7, "App", &captures, &facts, None, &[]).unwrap_err();
        assert_eq!(bubble.message, crate::messages::not_usable_window(3, "App"));
        let missing = choose_window(Some(9), 7, "App", &captures, &facts, None, &[]).unwrap_err();
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
            choose_window(None, 7, "App", &captures, &facts, Some(0), &[]).unwrap(),
            (1, 1)
        );
        // Only a small window: keep it usable as before rather than refuse the app.
        let only = [window(5, [0.0, 0.0, 100.0, 40.0], true)];
        let small = [ax([0.0, 0.0, 100.0, 40.0], "AXUnknown", false)];
        assert_eq!(
            choose_window(None, 7, "App", &only, &small, None, &[]).unwrap(),
            (0, 0)
        );
        let none = choose_window(None, 7, "App", &[], &[], None, &[]).unwrap_err();
        assert_eq!(none.message, crate::messages::no_window("App"));
    }

    #[test]
    fn unreadable_windows_are_skipped_and_the_lists_stay_aligned() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (kept, facts) = gather_facts(vec![1, 2, 3], deadline, |window, _| match window {
            // A helper whose frame cannot be read must not block the others.
            2 => None,
            // Unreadable subrole and minimized state read as absent and false.
            _ => Some(AxWindowFacts {
                id: Some(*window),
                frame: DOC,
                subrole: None,
                minimized: false,
            }),
        });
        assert_eq!(kept, vec![1, 3]);
        assert_eq!(
            facts.iter().map(|facts| facts.id).collect::<Vec<_>>(),
            vec![Some(1), Some(3)]
        );
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
            Some(ax(DOC, "AXStandardWindow", false))
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
