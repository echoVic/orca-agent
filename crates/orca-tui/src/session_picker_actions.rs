use std::io;
use std::time::Instant;

use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::protocol::UserAction;
use crate::types::{AppState, AppStatus, SessionPickerPhase};

pub(crate) const SESSION_PICKER_PAGE_SIZE: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionPickerAction {
    Resume,
    Fork,
    Rename,
    Archive,
    Delete,
    CopySessionId,
}

impl SessionPickerAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Resume => "Resume",
            Self::Fork => "Fork",
            Self::Rename => "Rename",
            Self::Archive => "Archive",
            Self::Delete => "Delete",
            Self::CopySessionId => "Copy session ID",
        }
    }
}

pub(crate) fn available_session_actions(
    current_session_id: Option<&str>,
    selected_session_id: &str,
) -> Vec<SessionPickerAction> {
    let mut actions = vec![
        SessionPickerAction::Resume,
        SessionPickerAction::Fork,
        SessionPickerAction::Rename,
    ];
    if current_session_id != Some(selected_session_id) {
        actions.extend([SessionPickerAction::Archive, SessionPickerAction::Delete]);
    }
    actions.push(SessionPickerAction::CopySessionId);
    actions
}

fn session_action_index(
    current_session_id: Option<&str>,
    selected_session_id: &str,
    action: SessionPickerAction,
) -> usize {
    available_session_actions(current_session_id, selected_session_id)
        .iter()
        .position(|candidate| *candidate == action)
        .expect("picker phase action must remain available")
}

pub(crate) fn handle_session_picker_key<F>(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let phase = state.session_picker_phase.clone();
    match phase {
        SessionPickerPhase::Browsing => match key.code {
            KeyCode::Up => state.select_previous_session(),
            KeyCode::Down => {
                load_next_page_if_at_end(state);
                state.select_next_session();
            }
            KeyCode::PageUp => state.select_session_page_up(),
            KeyCode::PageDown => {
                load_next_page_if_near_end(state, 10);
                state.select_session_page_down();
            }
            KeyCode::Home => state.select_first_session(),
            KeyCode::End => {
                load_next_session_page(state);
                state.select_last_session();
            }
            KeyCode::Backspace => {
                state.session_query_pop();
                reload_session_picker(state);
            }
            KeyCode::Char(c) => {
                state.session_query_push(c);
                reload_session_picker(state);
            }
            KeyCode::Enter => dispatch_selected_resume(state, action_tx, clear_terminal)?,
            KeyCode::Tab => {
                if let Some(session_id) = state.selected_session_id() {
                    state.session_picker_phase = SessionPickerPhase::Actions {
                        session_id,
                        selected: 0,
                    };
                }
            }
            KeyCode::Esc => close_picker(state),
            _ => {}
        },
        SessionPickerPhase::Actions {
            session_id,
            mut selected,
        } => match key.code {
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            KeyCode::Down => {
                let action_count =
                    available_session_actions(state.current_session_id(), &session_id).len();
                selected = (selected + 1).min(action_count.saturating_sub(1));
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            KeyCode::Enter => {
                activate_action(state, action_tx, session_id, selected, clear_terminal)?;
            }
            KeyCode::Esc | KeyCode::Tab => {
                state.session_picker_phase = SessionPickerPhase::Browsing;
            }
            _ => {}
        },
        SessionPickerPhase::Renaming {
            session_id,
            mut value,
        } => match key.code {
            KeyCode::Char(c) => {
                value.push(c);
                state.session_picker_phase = SessionPickerPhase::Renaming { session_id, value };
            }
            KeyCode::Backspace => {
                value.pop();
                state.session_picker_phase = SessionPickerPhase::Renaming { session_id, value };
            }
            KeyCode::Enter if !value.trim().is_empty() => {
                state.enter_running();
                let _ = action_tx.send(UserAction::RenameSavedSession {
                    session_id,
                    title: value.trim().to_string(),
                });
            }
            KeyCode::Esc => {
                let selected = session_action_index(
                    state.current_session_id(),
                    &session_id,
                    SessionPickerAction::Rename,
                );
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            _ => {}
        },
        SessionPickerPhase::ConfirmArchive {
            session_id,
            title,
            mut selected,
        } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                selected = 0;
                state.session_picker_phase = SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Right | KeyCode::Down => {
                selected = 1;
                state.session_picker_phase = SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Enter if selected == 1 => {
                state.enter_running();
                let _ = action_tx.send(UserAction::ArchiveSavedSession { session_id });
            }
            KeyCode::Enter | KeyCode::Esc => {
                let selected = session_action_index(
                    state.current_session_id(),
                    &session_id,
                    SessionPickerAction::Archive,
                );
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            _ => {}
        },
        SessionPickerPhase::ConfirmDelete {
            session_id,
            title,
            mut selected,
        } => match key.code {
            KeyCode::Left | KeyCode::Up => {
                selected = 0;
                state.session_picker_phase = SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Right | KeyCode::Down => {
                selected = 1;
                state.session_picker_phase = SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected,
                };
            }
            KeyCode::Enter if selected == 1 => {
                state.enter_running();
                let _ = action_tx.send(UserAction::DeleteSavedSession { session_id });
            }
            KeyCode::Enter | KeyCode::Esc => {
                let selected = session_action_index(
                    state.current_session_id(),
                    &session_id,
                    SessionPickerAction::Delete,
                );
                state.session_picker_phase = SessionPickerPhase::Actions {
                    session_id,
                    selected,
                };
            }
            _ => {}
        },
    }
    Ok(())
}

fn dispatch_selected_resume<F>(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let Some(session_id) = state.selected_session_id() else {
        return Ok(());
    };
    clear_terminal()?;
    state.enter_running();
    let _ = action_tx.send(UserAction::ResumeSavedSession { session_id });
    Ok(())
}

fn activate_action<F>(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    session_id: String,
    selected: usize,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let Some(action) = available_session_actions(state.current_session_id(), &session_id)
        .get(selected)
        .copied()
    else {
        state.session_picker_phase = SessionPickerPhase::Browsing;
        return Ok(());
    };
    match action {
        SessionPickerAction::Resume => {
            clear_terminal()?;
            state.enter_running();
            let _ = action_tx.send(UserAction::ResumeSavedSession { session_id });
        }
        SessionPickerAction::Fork => {
            state.enter_running();
            let _ = action_tx.send(UserAction::ForkSavedSession { session_id });
        }
        SessionPickerAction::Rename => {
            state.session_picker_phase = SessionPickerPhase::Renaming {
                session_id,
                value: String::new(),
            };
        }
        SessionPickerAction::Archive | SessionPickerAction::Delete => {
            let title = state
                .session_picker_sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.title.clone())
                .unwrap_or_else(|| session_id.clone());
            state.session_picker_phase = if action == SessionPickerAction::Archive {
                SessionPickerPhase::ConfirmArchive {
                    session_id,
                    title,
                    selected: 0,
                }
            } else {
                SessionPickerPhase::ConfirmDelete {
                    session_id,
                    title,
                    selected: 0,
                }
            };
        }
        SessionPickerAction::CopySessionId => {
            state.stage_clipboard_copy(session_id, Instant::now());
            state.session_picker_phase = SessionPickerPhase::Browsing;
        }
    }
    Ok(())
}

fn close_picker(state: &mut AppState) {
    state.set_status(AppStatus::Idle);
    state.session_picker_sessions.clear();
    state.session_picker_query.clear();
    state.session_picker_phase = SessionPickerPhase::Browsing;
    state.session_picker_error = None;
    state.session_picker_next_offset = None;
    state.session_picker_backfill_complete = true;
}

pub(crate) fn open_session_picker(state: &mut AppState) -> io::Result<bool> {
    let page =
        RuntimeSurfaceHostHandle::list_saved_session_page(0, SESSION_PICKER_PAGE_SIZE, None)?;
    state.reset_queued_user_messages();
    state.session_picker_sessions = page.sessions;
    state.session_picker_selected = 0;
    state.session_picker_query.clear();
    state.session_picker_phase = SessionPickerPhase::Browsing;
    state.session_picker_error = None;
    state.session_picker_next_offset = page.next_offset;
    state.session_picker_backfill_complete = page.backfill_complete;
    if state.session_picker_sessions.is_empty() {
        return Ok(false);
    }
    state.status = AppStatus::SessionPicker;
    Ok(true)
}

pub(crate) fn load_next_session_page(state: &mut AppState) -> usize {
    if !state.session_picker_backfill_complete {
        refresh_after_backfill(state);
    }
    let Some(offset) = state.session_picker_next_offset else {
        return 0;
    };
    let query =
        (!state.session_picker_query.is_empty()).then_some(state.session_picker_query.as_str());
    match RuntimeSurfaceHostHandle::list_saved_session_page(offset, SESSION_PICKER_PAGE_SIZE, query)
    {
        Ok(page) => {
            let mut seen = state
                .session_picker_sessions
                .iter()
                .map(|session| session.session_id.clone())
                .collect::<std::collections::HashSet<_>>();
            let before = state.session_picker_sessions.len();
            state.session_picker_sessions.extend(
                page.sessions
                    .into_iter()
                    .filter(|session| seen.insert(session.session_id.clone())),
            );
            state.session_picker_next_offset = page.next_offset;
            state.session_picker_backfill_complete = page.backfill_complete;
            state.session_picker_error = None;
            state.session_picker_sessions.len().saturating_sub(before)
        }
        Err(error) => {
            state.session_picker_error =
                Some(format!("failed to load more conversations: {error}"));
            0
        }
    }
}

fn refresh_after_backfill(state: &mut AppState) {
    let query =
        (!state.session_picker_query.is_empty()).then_some(state.session_picker_query.as_str());
    let Ok(page) =
        RuntimeSurfaceHostHandle::list_saved_session_page(0, SESSION_PICKER_PAGE_SIZE, query)
    else {
        return;
    };
    if !page.backfill_complete {
        return;
    }
    let selected_id = state.selected_session_id();
    state.session_picker_sessions = page.sessions;
    state.session_picker_next_offset = page.next_offset;
    state.session_picker_backfill_complete = true;
    state.session_picker_selected = selected_id
        .as_deref()
        .and_then(|selected_id| {
            state
                .session_picker_sessions
                .iter()
                .position(|session| session.session_id == selected_id)
        })
        .unwrap_or(0);
}

fn reload_session_picker(state: &mut AppState) {
    let query =
        (!state.session_picker_query.is_empty()).then_some(state.session_picker_query.as_str());
    match RuntimeSurfaceHostHandle::list_saved_session_page(0, SESSION_PICKER_PAGE_SIZE, query) {
        Ok(page) => {
            state.session_picker_sessions = page.sessions;
            state.session_picker_next_offset = page.next_offset;
            state.session_picker_backfill_complete = page.backfill_complete;
            state.session_picker_error = None;
            state.session_picker_selected = 0;
        }
        Err(error) => {
            state.session_picker_error =
                Some(format!("failed to search saved conversations: {error}"));
        }
    }
}

fn load_next_page_if_at_end(state: &mut AppState) {
    let filtered = state.filtered_session_indices();
    if filtered.last().copied() == Some(state.session_picker_selected) {
        load_next_session_page(state);
    }
}

fn load_next_page_if_near_end(state: &mut AppState, distance: usize) {
    let filtered = state.filtered_session_indices();
    let position = filtered
        .iter()
        .position(|index| *index == state.session_picker_selected)
        .unwrap_or(0);
    if position.saturating_add(distance) >= filtered.len().saturating_sub(1) {
        load_next_session_page(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crossterm::event::KeyModifiers;
    use orca_runtime::history::SessionSummary;

    fn session(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: ".".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: std::path::PathBuf::new(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    fn state() -> (AppState, mpsc::Receiver<UserAction>) {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(tx.clone(), "test".into(), "auto".into(), ".".into());
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = vec![session("one", "First"), session("two", "Second")];
        state.session_picker_selected = 1;
        (state, rx)
    }

    fn press(code: KeyCode, state: &mut AppState) {
        let tx = state.event_tx.clone();
        handle_session_picker_key(&KeyEvent::new(code, KeyModifiers::NONE), state, &tx, || {
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn session_picker_actions_capture_selected_session_id() {
        let (mut state, _) = state();
        press(KeyCode::Tab, &mut state);
        assert_eq!(
            state.session_picker_phase,
            SessionPickerPhase::Actions {
                session_id: "two".to_string(),
                selected: 0,
            }
        );

        state.session_picker_selected = 0;
        press(KeyCode::Down, &mut state);
        press(KeyCode::Down, &mut state);
        press(KeyCode::Enter, &mut state);
        assert!(matches!(
            state.session_picker_phase,
            SessionPickerPhase::Renaming { ref session_id, .. } if session_id == "two"
        ));
    }

    #[test]
    fn session_picker_delete_confirmation_uses_captured_id() {
        let (mut state, rx) = state();
        press(KeyCode::Tab, &mut state);
        for _ in 0..4 {
            press(KeyCode::Down, &mut state);
        }
        press(KeyCode::Enter, &mut state);
        state.session_picker_selected = 0;
        press(KeyCode::Right, &mut state);
        press(KeyCode::Enter, &mut state);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::DeleteSavedSession { session_id }) if session_id == "two"
        ));
    }

    #[test]
    fn current_session_actions_exclude_archive_and_delete() {
        assert_eq!(
            available_session_actions(Some("two"), "two"),
            vec![
                SessionPickerAction::Resume,
                SessionPickerAction::Fork,
                SessionPickerAction::Rename,
                SessionPickerAction::CopySessionId,
            ]
        );

        let (mut state, rx) = state();
        state.replace_session_identity_for_test(
            Some("two".to_string()),
            Some("Current session".to_string()),
        );
        press(KeyCode::Tab, &mut state);
        for _ in 0..8 {
            press(KeyCode::Down, &mut state);
        }
        assert_eq!(
            state.session_picker_phase,
            SessionPickerPhase::Actions {
                session_id: "two".to_string(),
                selected: 3,
            }
        );
        press(KeyCode::Enter, &mut state);

        assert_eq!(state.session_picker_phase, SessionPickerPhase::Browsing);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn session_action_indices_follow_available_actions() {
        assert_eq!(
            session_action_index(None, "two", SessionPickerAction::Rename),
            2
        );
        assert_eq!(
            session_action_index(None, "two", SessionPickerAction::Archive),
            3
        );
        assert_eq!(
            session_action_index(None, "two", SessionPickerAction::Delete),
            4
        );
        assert_eq!(
            session_action_index(Some("two"), "two", SessionPickerAction::CopySessionId),
            3
        );
    }
}
