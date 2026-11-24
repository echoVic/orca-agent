use crossbeam_channel as mpsc;
use std::io;
use std::time::{Duration, Instant};

use orca_runtime::termination_signals::TerminationSignal;

use crate::protocol::UserAction;
use crate::shortcuts::GlobalShortcut;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus};

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
                let _ = action_tx.send(UserAction::Interrupt);
                state.request_runtime_queue_pause();
                state.suspend_queued_follow_up_autosend();
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
            // Only agents with a thread are counted: a detached one goes on
            // after the quit, as it always has.
            let running_agents = state
                .workflow_tasks()
                .iter()
                .filter(|task| {
                    task.task_type == orca_core::task_types::TaskType::Subagent
                        && task.status.is_active()
                        && task.subagent_child_thread_id.is_some()
                })
                .count();
            state.push_message(ChatMessage::System {
                text: quit_line(running_agents),
                expanded: false,
            });
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
            state.viewport.scroll_offset = 0;
            state.viewport.auto_scroll = true;
            clear_terminal()?;
        }
    }
    Ok(GlobalShortcutFlow::Continue)
}

/// What the first Ctrl+C says, with the number of agents a quit would stop.
fn quit_line(running_agents: usize) -> String {
    match running_agents {
        0 => "Press Ctrl+C again to quit.".to_string(),
        1 => "1 agent is still running. Press Ctrl+C again to quit and stop it.".to_string(),
        n => format!("{n} agents are still running. Press Ctrl+C again to quit and stop them."),
    }
}

/// Quits as an ordinary exit does after `signal` (see `termination_signals`)
/// and returns the exit code that reports it, 128 plus its number: a turn
/// still running is interrupted first, as Ctrl+C does, unless it is the
/// turn of a daemon's session this TUI is attached to, which goes on without
/// it. After SIGHUP the terminal is gone.
pub(crate) fn quit_on_termination_signal(
    signal: TerminationSignal,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> i32 {
    let turn_running = matches!(
        state.status,
        AppStatus::Running
            | AppStatus::Compacting
            | AppStatus::WaitingApproval
            | AppStatus::WaitingUserInput
    );
    if turn_running && !state.attached_session {
        let _ = action_tx.send(UserAction::Interrupt);
        state.request_runtime_queue_pause();
        state.suspend_queued_follow_up_autosend();
    }
    if signal == TerminationSignal::Hangup {
        state.terminal_lost = true;
    }
    let _ = action_tx.send(UserAction::Cancel);
    signal.exit_code()
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use orca_core::task_types::{BackgroundTaskSummary, TaskLifetime, TaskStatus, TaskType};

    use super::handle_global_shortcut;
    use crate::protocol::{TuiEvent, UserAction};
    use crate::shortcuts::GlobalShortcut;
    use crate::transcript_state::ChatMessage;
    use crate::types::{AppState, AppStatus, SideParentStatus};

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
    fn the_quit_line_counts_agents_that_would_be_stopped() {
        assert_eq!(super::quit_line(0), "Press Ctrl+C again to quit.");
        assert_eq!(
            super::quit_line(1),
            "1 agent is still running. Press Ctrl+C again to quit and stop it."
        );
        assert_eq!(
            super::quit_line(2),
            "2 agents are still running. Press Ctrl+C again to quit and stop them."
        );
    }

    fn agent_task(
        id: &str,
        status: TaskStatus,
        child_thread_id: Option<&str>,
    ) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type: TaskType::Subagent,
            status,
            is_backgrounded: false,
            lifetime: TaskLifetime::Task,
            description: id.to_string(),
            created_at_ms: 1_000,
            sequence: 0,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("general".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(id.to_string()),
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: child_thread_id.map(str::to_string),
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(1),
        }
    }

    #[test]
    fn the_first_ctrl_c_says_how_many_agents_are_running() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        );
        state.replace_workflow_tasks_for_test(vec![
            agent_task("kept-1", TaskStatus::Running, Some("thread-1")),
            agent_task("kept-2", TaskStatus::Running, Some("thread-2")),
            agent_task("finished", TaskStatus::Completed, Some("thread-3")),
            // A detached agent goes on after the quit: it is not counted.
            agent_task("detached", TaskStatus::Running, None),
        ]);

        let flow =
            handle_global_shortcut(GlobalShortcut::Cancel, &mut state, &action_tx, || Ok(()))
                .expect("first ctrl+c");

        assert!(matches!(flow, super::GlobalShortcutFlow::Continue));
        assert!(action_rx.try_recv().is_err(), "nothing is sent yet");
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::System { text, .. })
                if text == "2 agents are still running. Press Ctrl+C again to quit and stop them."
        ));
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
        assert_eq!(state.transcript.message_revisions.len(), 1);
        assert_eq!(state.transcript.render_cache.len(), 1);

        handle_global_shortcut(GlobalShortcut::ClearScreen, &mut state, &action_tx, || {
            Ok(())
        })
        .expect("clear screen");

        assert!(state.transcript.messages.is_empty());
        assert!(state.transcript.message_revisions.is_empty());
        assert_eq!(state.transcript.render_cache.len(), 0);
    }
}
