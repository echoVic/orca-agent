use crossbeam_channel as mpsc;
use std::io;
use std::time::{Duration, Instant};

use crate::shortcuts::GlobalShortcut;
use crate::types::{AppState, AppStatus, ChatMessage, UserAction};

pub(crate) enum GlobalShortcutFlow {
    Continue,
    Exit(i32),
}

pub(crate) fn handle_global_shortcut<F>(
    shortcut: GlobalShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    clear_terminal: F,
) -> io::Result<GlobalShortcutFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    match shortcut {
        GlobalShortcut::Cancel => {
            if state.side_conversation_active() {
                let _ = action_tx.send(UserAction::CloseSideConversation);
                return Ok(GlobalShortcutFlow::Continue);
            }
            if matches!(
                state.status,
                AppStatus::Running
                    | AppStatus::Compacting
                    | AppStatus::WaitingApproval
                    | AppStatus::WaitingUserInput
            ) {
                state.suspend_queued_follow_up_autosend();
                let _ = action_tx.send(UserAction::Interrupt);
                return Ok(GlobalShortcutFlow::Continue);
            }
            let now = Instant::now();
            if state
                .last_ctrl_c
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
            {
                let _ = action_tx.send(UserAction::Cancel);
                return Ok(GlobalShortcutFlow::Exit(130));
            }
            state.last_ctrl_c = Some(now);
            state.push_message(ChatMessage::System("Press Ctrl+C again to quit.".into()));
            state.scroll_to_bottom();
        }
        GlobalShortcut::ToggleSideConversation => {
            let _ = action_tx.send(UserAction::ToggleSideConversation);
        }
        GlobalShortcut::OpenTranscriptSearch => {
            state.open_transcript_search();
        }
        GlobalShortcut::ToggleShortcuts => {
            state.toggle_shortcuts();
        }
        GlobalShortcut::ScrollBottom => {
            state.scroll_to_bottom();
        }
        GlobalShortcut::ScrollTop => {
            state.scroll_to_top();
        }
        GlobalShortcut::ClearScreen => {
            state.clear_messages();
            state.scroll_offset = 0;
            state.auto_scroll = true;
            clear_terminal()?;
        }
    }
    Ok(GlobalShortcutFlow::Continue)
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;

    use super::handle_global_shortcut;
    use crate::shortcuts::GlobalShortcut;
    use crate::types::{AppState, AppStatus, ChatMessage, SideParentStatus, TuiEvent, UserAction};

    #[test]
    fn cancel_interrupts_while_context_is_compacting() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);

        handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
            .expect("cancel compaction");

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
        assert!(!state.queued_autosend_enabled());
    }

    #[test]
    fn cancel_interrupts_waiting_interactions_without_arming_idle_exit() {
        for status in [AppStatus::WaitingApproval, AppStatus::WaitingUserInput] {
            let (action_tx, action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "model".to_string(),
                "/tmp".to_string(),
            );
            state.set_status(status);

            let flow =
                handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
                    .expect("interrupt pending interaction");

            assert!(matches!(flow, super::GlobalShortcutFlow::Continue));
            assert!(state.last_ctrl_c.is_none());
            assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
        }
    }

    #[test]
    fn second_ctrl_c_exits_when_idle() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );

        let first =
            handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
                .unwrap();
        assert!(matches!(first, super::GlobalShortcutFlow::Continue));
        assert!(action_rx.try_recv().is_err());

        let second =
            handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
                .unwrap();

        assert!(matches!(second, super::GlobalShortcutFlow::Exit(130)));
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Cancel)));
    }

    #[test]
    fn ctrl_c_closes_side_without_interrupting_parent() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.update(TuiEvent::SideConversationChanged {
            active: true,
            available: true,
            parent_thread_id: "parent".to_string(),
            parent_title: "main".to_string(),
            parent_status: SideParentStatus::Running,
        });

        let flow =
            handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
                .expect("close side");

        assert!(matches!(flow, super::GlobalShortcutFlow::Continue));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::CloseSideConversation)
        ));
        assert!(state.last_ctrl_c.is_none());
    }

    #[test]
    fn clear_screen_atomically_clears_messages_revisions_and_render_cache() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(ChatMessage::Assistant("cached".to_string()));
        assert_eq!(state.message_revisions.len(), 1);
        assert_eq!(state.transcript_render_cache.len(), 1);

        handle_global_shortcut(GlobalShortcut::ClearScreen, &mut state, &action_tx, || {
            Ok(())
        })
        .expect("clear screen");

        assert!(state.messages.is_empty());
        assert!(state.message_revisions.is_empty());
        assert_eq!(state.transcript_render_cache.len(), 0);
    }
}
