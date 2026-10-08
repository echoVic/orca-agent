use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_runtime::mentions;

use crate::composer_textarea::{
    composer_input, make_textarea, make_textarea_with_text, make_textarea_with_text_at_cursor,
    textarea_cursor_byte_index, textarea_text,
};
use crate::shortcuts::{EditorShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::slash_menu_actions::update_slash_menu;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus};
use crate::vim::VimState;

pub(crate) fn refresh_input_menus(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    if state.status == AppStatus::Idle {
        update_slash_menu(textarea, state, config);
    } else {
        state.slash_menu = None;
    }
}

pub(crate) fn insert_composer_newline(textarea: &mut TextArea, state: &mut AppState) {
    textarea.insert_newline();
    state.reset_history_navigation();
}

pub(crate) fn clear_composer_input(
    textarea: &mut TextArea,
    state: &mut AppState,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if (textarea.is_empty() && state.composer_images.is_empty())
        || (vim_state.enabled && vim_state.mode != crate::vim::VimMode::Insert)
    {
        return false;
    }

    let visible_text = textarea_text(textarea);
    let images = state.composer_images.attachments_for_text(&visible_text);
    state.cleared_draft = Some(crate::queued_input::QueuedComposerState {
        visible_text,
        mention_bindings: std::mem::take(&mut state.mention_bindings),
        pending_pastes: std::mem::take(&mut state.pending_pastes),
        images,
    });
    state.slash_menu = None;
    state.mention.clear_projection();
    state.composer_images.clear_attachments();
    state.atomic_skill_tokens.clear();
    state.reset_history_navigation();
    vim_state.cancel_pending_command();
    *textarea = make_textarea(vim_state, theme);
    textarea.set_placeholder_text("Draft cleared · ↑ brings it back");
    true
}

pub(crate) fn composer_editor_shortcut_is_active(
    key: KeyEvent,
    composer_has_text: bool,
    vim_state: &VimState,
) -> bool {
    let insert_like = !vim_state.enabled || vim_state.mode == crate::vim::VimMode::Insert;
    match resolve_shortcut(ShortcutContext::Editor, key) {
        Some(ShortcutAction::Editor(EditorShortcut::VimEscape)) => vim_state.enabled,
        Some(ShortcutAction::Editor(_)) => insert_like && composer_has_text,
        // A printable character with text already present is always typed,
        // never treated as a global shortcut (this is what makes `?` safe).
        _ => {
            composer_has_text
                && insert_like
                && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                && matches!(key.code, KeyCode::Char(_))
        }
    }
}

pub(crate) fn sync_vim_mode_label(state: &mut AppState, vim_state: &VimState) {
    state.vim_mode_label = vim_state.status_label();
}

pub(crate) fn handle_composer_editor_shortcut(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if !composer_editor_shortcut_is_active(
        *key,
        !textarea.is_empty() || !state.composer_images.is_empty(),
        vim_state,
    ) {
        return false;
    }

    let Some(ShortcutAction::Editor(shortcut)) = resolve_shortcut(ShortcutContext::Editor, *key)
    else {
        return false;
    };

    // Up/Down retain shell-style history or transcript navigation for a
    // single-line draft. In a multiline draft the textarea owns them.
    if matches!(key.code, KeyCode::Up | KeyCode::Down) && textarea.lines().len() <= 1 {
        return false;
    }

    match shortcut {
        EditorShortcut::ClearInput => {
            clear_composer_input(textarea, state, vim_state, theme);
        }
        _ => {
            // Ownership is semantic, not based on whether the cursor moved. At
            // a line boundary the editor still consumes Ctrl+B/F/K/D instead
            // of allowing an unrelated application action to run.
            apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
        }
    }
    true
}

pub(crate) fn recall_previous_history(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    if key.code == KeyCode::Up && textarea.lines().len() > 1 {
        composer_input(textarea, Input::from(ev.clone()));
    } else if textarea.is_empty()
        && let Some(cleared) = state.cleared_draft.take()
    {
        // The draft Esc or Ctrl+U just cleared comes back before the history.
        *textarea = make_textarea_with_text(&cleared.visible_text, vim_state, theme);
        state.mention_bindings = cleared.mention_bindings;
        state.pending_pastes = cleared.pending_pastes;
        state.composer_images.restore(cleared.images);
        state.atomic_skill_tokens.clear();
        state.reset_history_navigation();
    } else {
        let draft = textarea_text(textarea);
        if let Some(history) = state.history_previous(draft) {
            state.atomic_skill_tokens.clear();
            state.composer_images.clear_attachments();
            *textarea = make_textarea_with_text(&history, vim_state, theme);
        }
    }
}

pub(crate) fn recall_next_history(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    if key.code == KeyCode::Down && textarea.lines().len() > 1 {
        composer_input(textarea, Input::from(ev.clone()));
    } else if let Some(history) = state.history_next() {
        state.atomic_skill_tokens.clear();
        state.composer_images.clear_attachments();
        *textarea = make_textarea_with_text(&history, vim_state, theme);
    }
}

pub(crate) fn apply_composer_key_input(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if delete_composer_image_attachment(key, state, textarea, vim_state, theme) {
        refresh_input_menus(textarea, state, config);
        return true;
    }
    if delete_atomic_skill_token(key, state, textarea, vim_state, theme) {
        refresh_input_menus(textarea, state, config);
        return true;
    }
    let normalized_event = normalize_windows_altgr_event(ev);
    let changed = if key.code == KeyCode::Tab {
        vim_state.cancel_pending_command();
        let text = textarea_text(textarea);
        let cursor = textarea_cursor_byte_index(textarea);
        let candidates = state
            .mention
            .candidates
            .iter()
            .map(|candidate| candidate.display.clone())
            .collect::<Vec<_>>();
        let token_is_current =
            mentions::mention_token_at_cursor(&text, cursor).is_some_and(|token| {
                state.mention.pending_query.as_deref() == Some(token.query.as_str())
            });
        if let Some(edit) = token_is_current
            .then(|| {
                mentions::complete_file_mention_from_candidates_at_cursor(
                    &text,
                    cursor,
                    &candidates,
                )
            })
            .flatten()
        {
            *textarea =
                make_textarea_with_text_at_cursor(&edit.text, edit.cursor, vim_state, theme);
            true
        } else {
            composer_input(textarea, Input::from(normalized_event.clone()))
        }
    } else if vim_state.enabled {
        vim_state.handle(Input::from(normalized_event.clone()), textarea, theme)
    } else {
        composer_input(textarea, Input::from(normalized_event))
    };
    sync_vim_mode_label(state, vim_state);
    if changed {
        state.composer_images.reconcile(&textarea_text(textarea));
        state
            .atomic_skill_tokens
            .reconcile(&textarea_text(textarea));
        state.reset_history_navigation();
        refresh_input_menus(textarea, state, config);
    }
    changed
}

fn delete_composer_image_attachment(
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    if vim_state.enabled && vim_state.mode != crate::vim::VimMode::Insert {
        return false;
    }
    let text = textarea_text(textarea);
    let cursor = textarea_cursor_byte_index(textarea);
    let Some((next, next_cursor)) = state.composer_images.remove_for_key(key, &text, cursor) else {
        return false;
    };
    state.mention_bindings.reconcile(&next);
    state.atomic_skill_tokens.reconcile(&next);
    *textarea = make_textarea_with_text_at_cursor(&next, next_cursor, vim_state, theme);
    state.reset_history_navigation();
    true
}

pub(crate) fn delete_atomic_skill_token(
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    if !matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        || (vim_state.enabled && vim_state.mode != crate::vim::VimMode::Insert)
    {
        return false;
    }
    let text = textarea_text(textarea);
    state.atomic_skill_tokens.reconcile(&text);
    let cursor = textarea_cursor_byte_index(textarea);
    let range = state
        .atomic_skill_tokens
        .bindings()
        .iter()
        .find(|binding| match key.code {
            KeyCode::Backspace => binding.start < cursor && cursor <= binding.end,
            KeyCode::Delete => binding.start <= cursor && cursor < binding.end,
            _ => false,
        })
        .map(|binding| binding.start..binding.end);
    let Some(range) = range else {
        return false;
    };

    let mut next = text;
    next.replace_range(range.clone(), "");
    state.atomic_skill_tokens.reconcile(&next);
    state.mention_bindings.reconcile(&next);
    state.mention.clear_projection();
    state.reset_history_navigation();
    *textarea = make_textarea_with_text_at_cursor(&next, range.start, vim_state, theme);
    true
}

fn normalize_windows_altgr_event(ev: &Event) -> Event {
    #[cfg(windows)]
    if let Event::Key(key) = ev
        && matches!(key.code, KeyCode::Char(_))
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT)
    {
        let mut normalized = *key;
        normalized
            .modifiers
            .remove(crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT);
        return Event::Key(normalized);
    }

    ev.clone()
}

#[cfg(test)]
mod windows_input_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn altgr_char_is_normalized_to_text_input_on_windows_only() {
        let event = Event::Key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        let normalized = normalize_windows_altgr_event(&event);

        if cfg!(windows) {
            assert_eq!(
                normalized,
                Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE))
            );
        } else {
            assert_eq!(normalized, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;
    use orca_runtime::mentions::{MentionBinding, MentionBindings, MentionTarget};
    use std::path::PathBuf;

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::types::{AppState, AppStatus};

    fn bind_atomic_skill(state: &mut AppState, text: &str, visible: &str) {
        let start = text.find(visible).unwrap();
        state.atomic_skill_tokens = MentionBindings::from_bindings(
            text,
            vec![MentionBinding {
                start,
                end: start + visible.len(),
                visible: visible.to_string(),
                target: MentionTarget::Skill {
                    id: visible.trim_start_matches('$').to_string(),
                    path: PathBuf::from("/skills/test/SKILL.md"),
                },
            }],
        );
    }

    fn editor_fixture(
        text: &str,
        cursor: usize,
        vim_enabled: bool,
    ) -> (AppState, RunConfig, Theme, VimState, TextArea<'static>) {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(vim_enabled);
        if vim_enabled {
            vim.mode = crate::vim::VimMode::Insert;
        }
        let textarea = make_textarea_with_text_at_cursor(text, cursor, &vim, &theme);
        (state, config, theme, vim, textarea)
    }

    #[test]
    fn question_mark_is_typed_when_the_composer_has_text() {
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(composer_editor_shortcut_is_active(
            key,
            true,
            &VimState::new(false)
        ));
        assert!(!composer_editor_shortcut_is_active(
            key,
            false,
            &VimState::new(false)
        ));
    }

    #[test]
    fn editor_shortcuts_apply_readline_navigation_and_deletion() {
        let (mut state, config, theme, mut vim, mut textarea) =
            editor_fixture("first second", "first second".len(), false);
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(handle_composer_editor_shortcut(
            &Event::Key(ctrl_b),
            &ctrl_b,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_cursor_byte_index(&textarea), "first secon".len());

        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(handle_composer_editor_shortcut(
            &Event::Key(ctrl_w),
            &ctrl_w,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "first d");

        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert!(handle_composer_editor_shortcut(
            &Event::Key(ctrl_a),
            &ctrl_a,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_cursor_byte_index(&textarea), 0);

        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(handle_composer_editor_shortcut(
            &Event::Key(ctrl_d),
            &ctrl_d,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "irst d");
    }

    #[test]
    fn word_deletion_keeps_an_identifier_with_underscores_together() {
        for (text, left) in [
            ("cargo test foo_bar", "cargo test "),
            ("提交 foo_bar", "提交 "),
        ] {
            let (mut state, config, theme, mut vim, mut textarea) =
                editor_fixture(text, text.len(), false);
            let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
            assert!(handle_composer_editor_shortcut(
                &Event::Key(ctrl_w),
                &ctrl_w,
                &mut state,
                &config,
                &mut textarea,
                &mut vim,
                &theme,
            ));
            assert_eq!(textarea_text(&textarea), left, "{text:?}");
        }
    }

    type EditorFixture = (AppState, RunConfig, Theme, VimState, TextArea<'static>);

    /// Sends one key the way the idle and running key paths do: an editor
    /// shortcut first, otherwise straight to the composer (or vim).
    fn press(fixture: &mut EditorFixture, code: KeyCode, modifiers: KeyModifiers) {
        let (state, config, theme, vim, textarea) = fixture;
        let key = KeyEvent::new(code, modifiers);
        let event = Event::Key(key);
        if !handle_composer_editor_shortcut(&event, &key, state, config, textarea, vim, theme) {
            apply_composer_key_input(&event, &key, state, config, textarea, vim, theme);
        }
    }

    fn press_all(fixture: &mut EditorFixture, keys: &[(KeyCode, KeyModifiers)]) {
        for &(code, modifiers) in keys {
            press(fixture, code, modifiers);
        }
    }

    const PLAIN: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;

    /// Runs `keys` on `text` with the cursor at byte `cursor`, then types X.
    /// Returns where the keys left the cursor and the text after the X.
    fn cursor_then_typed(
        text: &str,
        cursor: usize,
        vim: bool,
        keys: &[(KeyCode, KeyModifiers)],
    ) -> ((usize, usize), String) {
        let mut fixture = editor_fixture(text, cursor, vim);
        press_all(&mut fixture, keys);
        let landed = fixture.4.cursor();
        if vim {
            press(&mut fixture, KeyCode::Char('i'), PLAIN);
        }
        press(&mut fixture, KeyCode::Char('X'), PLAIN);
        ((landed.0, landed.1), textarea_text(&fixture.4))
    }

    /// Every case whose cursor or typed text differs from the expectation.
    fn mismatches(
        cases: impl IntoIterator<Item = (String, ((usize, usize), String), ((usize, usize), String))>,
    ) -> Vec<String> {
        cases
            .into_iter()
            .filter(|(_, actual, expected)| actual != expected)
            .map(|(case, actual, expected)| format!("{case}: got {actual:?}, want {expected:?}"))
            .collect()
    }

    /// A line ending in characters that take no screen column: a variation
    /// selector (⚠️, ❤️), Hangul jamo (한 written as three code points), a
    /// zero-width space. The number is the line's length in characters.
    const LINES_ENDING_IN_ZERO_WIDTH: [(&str, usize); 4] = [
        ("ok \u{26a0}\u{fe0f}", 5),
        ("thanks \u{2764}\u{fe0f}", 9),
        ("\u{1112}\u{1161}\u{11ab}", 3),
        ("abc\u{200b}", 4),
    ];

    #[test]
    fn right_steps_over_a_zero_width_character_at_the_end_of_a_line() {
        let wrong = mismatches(
            LINES_ENDING_IN_ZERO_WIDTH
                .into_iter()
                .flat_map(|(text, len)| {
                    [
                        ("Right", KeyCode::Right, PLAIN),
                        ("Ctrl+F", KeyCode::Char('f'), CTRL),
                    ]
                    .map(|(name, code, modifiers)| {
                        (
                            format!("{text:?} Left, {name}"),
                            cursor_then_typed(
                                text,
                                text.len(),
                                false,
                                &[(KeyCode::Left, PLAIN), (code, modifiers)],
                            ),
                            ((0, len), format!("{text}X")),
                        )
                    })
                }),
        );
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn vim_l_steps_over_a_zero_width_character_at_the_end_of_a_line() {
        let keys = [
            (KeyCode::Esc, PLAIN),
            (KeyCode::Char('h'), PLAIN),
            (KeyCode::Char('l'), PLAIN),
        ];
        let wrong = mismatches(LINES_ENDING_IN_ZERO_WIDTH.into_iter().map(|(text, len)| {
            (
                format!("{text:?} vim h, l"),
                cursor_then_typed(text, text.len(), true, &keys),
                ((0, len), format!("{text}X")),
            )
        }));
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn right_leaves_a_line_only_from_its_true_end() {
        let text = "ok \u{26a0}\u{fe0f}\nnext";
        let line_end = "ok \u{26a0}\u{fe0f}".len();
        let wrong = mismatches([
            (
                "Left, Right before the variation selector".to_string(),
                cursor_then_typed(
                    text,
                    line_end,
                    false,
                    &[(KeyCode::Left, PLAIN), (KeyCode::Right, PLAIN)],
                ),
                ((0, 5), "ok \u{26a0}\u{fe0f}X\nnext".to_string()),
            ),
            (
                "Right at the true end".to_string(),
                cursor_then_typed(text, line_end, false, &[(KeyCode::Right, PLAIN)]),
                ((1, 0), "ok \u{26a0}\u{fe0f}\nXnext".to_string()),
            ),
        ]);
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn left_from_a_line_start_lands_after_the_whole_line_above() {
        let wrong = mismatches(LINES_ENDING_IN_ZERO_WIDTH.into_iter().map(|(text, len)| {
            (
                format!("{text:?} then a line, Left from its start"),
                cursor_then_typed(
                    &format!("{text}\nnext"),
                    text.len() + 1,
                    false,
                    &[(KeyCode::Left, PLAIN)],
                ),
                ((0, len), format!("{text}X\nnext")),
            )
        }));
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn up_and_down_never_leave_the_cursor_inside_a_character() {
        // Down from the end of `abcdefgh` onto `ok ⚠️`, and Up from the end of
        // `abcd` onto 한 written as three code points.
        let down_text = "abcdefgh\nok \u{26a0}\u{fe0f}";
        let down = ((1, 5), format!("{down_text}X"));
        let up_text = "\u{1112}\u{1161}\u{11ab}\nabcd";
        let up = ((0, 3), "\u{1112}\u{1161}\u{11ab}X\nabcd".to_string());
        let mut cases = Vec::new();
        for (name, keys) in [
            ("Down", vec![(KeyCode::Down, PLAIN)]),
            ("Ctrl+N", vec![(KeyCode::Char('n'), CTRL)]),
        ] {
            cases.push((
                name.to_string(),
                cursor_then_typed(down_text, 8, false, &keys),
                down.clone(),
            ));
        }
        cases.push((
            "vim j".to_string(),
            cursor_then_typed(
                down_text,
                8,
                true,
                &[(KeyCode::Esc, PLAIN), (KeyCode::Char('j'), PLAIN)],
            ),
            down,
        ));
        for (name, keys) in [
            ("Up", vec![(KeyCode::Up, PLAIN)]),
            ("Ctrl+P", vec![(KeyCode::Char('p'), CTRL)]),
        ] {
            cases.push((
                name.to_string(),
                cursor_then_typed(up_text, up_text.len(), false, &keys),
                up.clone(),
            ));
        }
        cases.push((
            "vim k".to_string(),
            cursor_then_typed(
                up_text,
                up_text.len(),
                true,
                &[(KeyCode::Esc, PLAIN), (KeyCode::Char('k'), PLAIN)],
            ),
            up,
        ));
        let wrong = mismatches(cases);
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn vim_insert_escape_is_owned_by_editor_even_when_draft_is_empty() {
        let (mut state, config, theme, mut vim, mut textarea) = editor_fixture("", 0, true);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        assert!(handle_composer_editor_shortcut(
            &Event::Key(esc),
            &esc,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(vim.mode, crate::vim::VimMode::Normal);
    }

    #[test]
    fn selected_skill_backspace_deletes_the_atomic_name_not_one_character() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let text = "$algorithmic-art ";
        bind_atomic_skill(&mut state, text, "$algorithmic-art");
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text(text, &vim, &theme);
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea_text(&textarea), "$algorithmic-art");

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea_text(&textarea), "");
        assert!(state.atomic_skill_tokens.is_empty());
    }

    #[test]
    fn manually_typed_skill_like_text_keeps_character_deletion() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("$algorithmic-art", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "$algorithmic-ar");
    }

    #[test]
    fn delete_inside_selected_skill_removes_the_whole_atomic_name() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let text = "use $algorithmic-art next";
        bind_atomic_skill(&mut state, text, "$algorithmic-art");
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let cursor = text.find("$algorithmic-art").unwrap() + 5;
        let mut textarea = make_textarea_with_text_at_cursor(text, cursor, &vim, &theme);
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "use  next");
        assert!(state.atomic_skill_tokens.is_empty());
    }

    #[test]
    fn running_slash_text_never_opens_local_command_menu() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Running;
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/compac", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "/compact");
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn tab_clears_pending_vim_prefix_before_direct_textarea_input() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = crate::test_support::test_run_config();
        config.vim_mode = true;
        let theme = Theme::named(ThemeName::Dark);
        let mut expected_vim = VimState::new(true);
        let mut expected = make_textarea_with_text("abcd", &expected_vim, &theme);
        expected.move_cursor(ratatui_textarea::CursorMove::Head);
        let mut vim = VimState::new(true);
        let mut textarea = make_textarea_with_text("abcd", &vim, &theme);
        textarea.move_cursor(ratatui_textarea::CursorMove::Head);

        for code in [KeyCode::Tab, KeyCode::Char('x')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            apply_composer_key_input(
                &Event::Key(key),
                &key,
                &mut state,
                &config,
                &mut expected,
                &mut expected_vim,
                &theme,
            );
        }

        for code in [KeyCode::Char('2'), KeyCode::Tab, KeyCode::Char('x')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            apply_composer_key_input(
                &Event::Key(key),
                &key,
                &mut state,
                &config,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea_text(&textarea), textarea_text(&expected));
        assert_eq!(textarea.cursor(), expected.cursor());
        assert!(!vim.has_pending_command_for_test());
    }
}
