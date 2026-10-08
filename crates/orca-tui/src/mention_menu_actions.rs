use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui_textarea::{Input, TextArea};

use orca_runtime::mentions;

use crate::composer_input_actions::delete_atomic_skill_token;
use crate::composer_textarea::{
    composer_input, make_textarea_with_text_at_cursor, textarea_cursor_byte_index, textarea_text,
};
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

const MENTION_PAGE_SIZE: usize = 12;

pub(crate) fn handle_mention_menu_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    if delete_atomic_skill_token(key, state, textarea, vim_state, theme) {
        return true;
    }
    match key.code {
        KeyCode::Up => {
            state.mention.selected = state.mention.selected.saturating_sub(1);
            mark_manual_selection(state);
            true
        }
        KeyCode::Down => {
            let max = state.mention.candidates.len().saturating_sub(1);
            if state.mention.selected < max {
                state.mention.selected += 1;
            }
            mark_manual_selection(state);
            true
        }
        KeyCode::PageUp => {
            state.mention.selected = state.mention.selected.saturating_sub(MENTION_PAGE_SIZE);
            mark_manual_selection(state);
            true
        }
        KeyCode::PageDown => {
            let max = state.mention.candidates.len().saturating_sub(1);
            state.mention.selected = state
                .mention
                .selected
                .saturating_add(MENTION_PAGE_SIZE)
                .min(max);
            mark_manual_selection(state);
            true
        }
        KeyCode::Home => {
            state.mention.selected = 0;
            mark_manual_selection(state);
            true
        }
        KeyCode::End => {
            state.mention.selected = state.mention.candidates.len().saturating_sub(1);
            mark_manual_selection(state);
            true
        }
        KeyCode::Tab | KeyCode::Enter => {
            if let Some(candidate) = state
                .mention
                .candidates
                .get(state.mention.selected)
                .cloned()
            {
                let text = textarea_text(textarea);
                let cursor = textarea_cursor_byte_index(textarea);
                let token = mentions::mention_token_at_cursor(&text, cursor);
                if let (Some(token), Some(edit)) = (
                    token,
                    mentions::apply_mention_selection_at_cursor(&text, cursor, &candidate.display),
                ) {
                    *textarea = make_textarea_with_text_at_cursor(
                        &edit.text,
                        edit.cursor,
                        vim_state,
                        theme,
                    );
                    if token.sigil == mentions::MentionSigil::Dollar {
                        state.atomic_skill_tokens.apply_selection(
                            &text,
                            &edit,
                            candidate.target.clone(),
                        );
                    } else if !candidate.is_directory() {
                        state.mention_bindings.apply_selection(
                            &text,
                            &edit,
                            candidate.target.clone(),
                        );
                    }
                    state.mention.clear_projection();
                }
            }
            true
        }
        KeyCode::Esc => {
            let text = textarea_text(textarea);
            let cursor = textarea_cursor_byte_index(textarea);
            state.mention.dismissed_query =
                mentions::mention_token_at_cursor(&text, cursor).map(|token| token.query);
            state.mention.clear_projection();
            true
        }
        _ => {
            composer_input(textarea, Input::from(ev.clone()));
            true
        }
    }
}

pub(crate) fn mark_manual_selection(state: &mut AppState) {
    state.mention.manual_selection = true;
    state.mention.selected_identity = state
        .mention
        .candidates
        .get(state.mention.selected)
        .map(|candidate| candidate.id.clone());
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use std::path::PathBuf;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;
    use orca_file_search::{MatchKind, SearchMatch};
    use orca_runtime::mentions::{
        MentionCandidate, MentionFileKind, MentionKind, MentionSigil, MentionTarget,
    };

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};

    fn state() -> AppState {
        let (event_tx, _event_rx) = mpsc::unbounded();
        AppState::new(
            event_tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/workspace".to_string(),
        )
    }

    fn enter() -> (Event, KeyEvent) {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        (Event::Key(key), key)
    }

    #[test]
    fn selecting_file_inserts_visible_text_and_records_exact_target() {
        let mut state = state();
        state.mention.candidates = vec![MentionCandidate::from_file_match(&SearchMatch {
            root: PathBuf::from("/workspace/backend"),
            path: "same.txt".to_string(),
            kind: MatchKind::File,
            score: 42,
            indices: vec![0],
        })];
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("review @sa", &vim_state, &theme);
        let (event, key) = enter();

        assert!(handle_mention_menu_key(
            &event,
            &key,
            &mut state,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "review @same.txt ");
        assert_eq!(state.mention_bindings.bindings().len(), 1);
        assert_eq!(
            state.mention_bindings.bindings()[0].target,
            MentionTarget::File {
                root: PathBuf::from("/workspace/backend"),
                path: "same.txt".to_string(),
                kind: MentionFileKind::File,
            }
        );
        assert!(state.mention.candidates.is_empty());
    }

    #[test]
    fn selecting_directory_continues_browsing_without_atomic_binding() {
        let mut state = state();
        state.mention.candidates = vec![MentionCandidate::from_file_match(&SearchMatch {
            root: PathBuf::from("/workspace"),
            path: "src/".to_string(),
            kind: MatchKind::Directory,
            score: 42,
            indices: vec![0],
        })];
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("review @s", &vim_state, &theme);
        let (event, key) = enter();

        assert!(handle_mention_menu_key(
            &event,
            &key,
            &mut state,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "review @src/");
        assert!(state.mention_bindings.is_empty());
    }

    #[test]
    fn selecting_skill_records_atomic_composer_token_without_model_binding() {
        let mut state = state();
        state.mention.sigil = Some(MentionSigil::Dollar);
        state.mention.candidates = vec![MentionCandidate {
            id: "skill:code-review".to_string(),
            kind: MentionKind::Skill,
            display: "code-review".to_string(),
            description: "Review code".to_string(),
            score: 42,
            indices: vec![0],
            target: MentionTarget::Skill {
                id: "code-review".to_string(),
                path: PathBuf::from("/skills/code-review/SKILL.md"),
            },
        }];
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("$code", &vim_state, &theme);
        let (event, key) = enter();

        assert!(handle_mention_menu_key(
            &event,
            &key,
            &mut state,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "$code-review ");
        assert!(state.mention_bindings.is_empty());
        assert_eq!(state.atomic_skill_tokens.bindings().len(), 1);
        assert!(state.mention.candidates.is_empty());
    }

    #[test]
    fn skill_picker_supports_page_and_boundary_navigation() {
        let mut state = state();
        state.mention.sigil = Some(MentionSigil::Dollar);
        state.mention.candidates = (0..30)
            .map(|index| MentionCandidate {
                id: format!("skill:skill-{index:02}"),
                kind: MentionKind::Skill,
                display: format!("skill-{index:02}"),
                description: format!("Skill {index}"),
                score: 0,
                indices: Vec::new(),
                target: MentionTarget::Skill {
                    id: format!("skill-{index:02}"),
                    path: PathBuf::from(format!("/skills/skill-{index:02}/SKILL.md")),
                },
            })
            .collect();
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("$", &vim_state, &theme);

        for (code, expected) in [
            (KeyCode::PageDown, 12),
            (KeyCode::PageDown, 24),
            (KeyCode::PageDown, 29),
            (KeyCode::PageUp, 17),
            (KeyCode::Home, 0),
            (KeyCode::End, 29),
        ] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            assert!(handle_mention_menu_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut textarea,
                &vim_state,
                &theme,
            ));
            assert_eq!(state.mention.selected, expected);
        }
    }
}
