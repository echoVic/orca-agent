//! How shell commands that outlived their calls ended. A shell row keeps the
//! result its call returned; when the command was still running then, what
//! is learned later — a read of its output, a wait on it, the task list —
//! is kept here by task id and written into every row of that task.
//!
//! A conversation restored from history is settled the same way. A row whose
//! command this Orca did not see running, and that nothing reports on, shows
//! `state unknown`: the command ran in an earlier Orca.

use std::collections::{HashMap, HashSet};

use crate::terminal_output::{
    CommandEnd, EndSource, reported_ends, reported_task, task_list_end, unknown_end, with_end,
};
use crate::transcript_state::ChatMessage;
use crate::types::AppState;

/// What is known about the commands that outlived their calls: the most
/// telling end for each task, and the tasks this Orca saw running.
#[derive(Debug, Default)]
pub(crate) struct CommandEnds {
    known: HashMap<String, CommandEnd>,
    /// Tasks a live result showed running. A restored row of any other task
    /// belongs to a command of an earlier Orca.
    seen_running: HashSet<String>,
}

impl CommandEnds {
    /// Keeps `end` when it tells more than what is known; says whether it did.
    fn learn(&mut self, task_id: &str, end: &CommandEnd) -> bool {
        if self
            .known
            .get(task_id)
            .is_some_and(|known| known.source >= end.source)
        {
            return false;
        }
        self.known.insert(task_id.to_string(), end.clone());
        true
    }

    /// Remembers that a live result showed the command of `task_id` running.
    fn saw_running(&mut self, task_id: &str) {
        self.seen_running.insert(task_id.to_string());
    }

    pub(crate) fn clear(&mut self) {
        self.known.clear();
        self.seen_running.clear();
    }
}

/// Tools whose results are shell results, or report on shell tasks.
fn reports_on_commands(tool: &str) -> bool {
    matches!(
        tool,
        "bash" | "task_send_input" | "task_read_output" | "task_wait"
    )
}

impl AppState {
    /// Learns what the result of `tool` says about how commands ended, and
    /// which command it shows running. Only results arriving live come here:
    /// the rows of a restored conversation are settled apart.
    pub(crate) fn learn_command_ends_from_result(&mut self, tool: &str, output: &str) {
        if !reports_on_commands(tool) {
            return;
        }
        if let Some((task_id, None)) = reported_task(output) {
            self.command_ends.saw_running(&task_id);
        }
        for (task_id, end) in reported_ends(output) {
            self.learn_command_end(&task_id, &end);
        }
    }

    /// Learns which shell tasks the task list shows ended.
    pub(crate) fn learn_command_ends_from_tasks(&mut self) {
        let ends: Vec<_> = self
            .command_tasks()
            .iter()
            .filter_map(task_list_end)
            .collect();
        for (task_id, end) in ends {
            self.learn_command_end(&task_id, &end);
        }
    }

    /// Shows the end already known for the task of the row at `index`: for
    /// a row that arrives after its command was seen to end.
    pub(crate) fn show_known_command_end(&mut self, index: usize) {
        let Some((task_id, row_knows)) = self.command_row_task(index) else {
            return;
        };
        let Some(end) = self.command_ends.known.get(&task_id).cloned() else {
            return;
        };
        if row_knows.is_none_or(|known| known < end.source) {
            self.write_command_end(index, &end);
        }
    }

    /// Settles a restored transcript: what is already known, what later
    /// results in it say, then what the task list says. A row still running
    /// shows `state unknown` when the task list does not have its task and
    /// it is the row of a command this Orca did not see running: the command
    /// ran in an earlier Orca, and nothing says how it ended. The row of a
    /// command this Orca saw running, as when a conversation shown before is
    /// attached again, keeps showing it running.
    pub(crate) fn settle_restored_command_rows(&mut self) {
        let reported: Vec<_> = self
            .transcript
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::ToolCall {
                    name,
                    output: Some(output),
                    ..
                } if reports_on_commands(name) => Some(reported_ends(output)),
                _ => None,
            })
            .flatten()
            .collect();
        for (task_id, end) in reported {
            self.command_ends.learn(&task_id, &end);
        }
        let listed: Vec<_> = self
            .command_tasks()
            .iter()
            .filter_map(task_list_end)
            .collect();
        for (task_id, end) in listed {
            self.command_ends.learn(&task_id, &end);
        }
        let in_task_list: HashSet<String> = self
            .command_tasks()
            .iter()
            .map(|task| task.id.clone())
            .collect();
        for index in 0..self.transcript.messages.len() {
            self.show_known_command_end(index);
            if let Some((task_id, None)) = self.command_row_task(index)
                && !in_task_list.contains(&task_id)
                && !self.command_ends.seen_running.contains(&task_id)
            {
                let end = unknown_end();
                self.command_ends.learn(&task_id, &end);
                self.write_command_end(index, &end);
            }
        }
    }

    /// The task the shell row at `index` reports on, and how its end is known.
    fn command_row_task(&self, index: usize) -> Option<(String, Option<EndSource>)> {
        match self.transcript.messages.get(index)? {
            ChatMessage::ToolCall {
                name,
                output: Some(output),
                ..
            } if reports_on_commands(name) => reported_task(output),
            _ => None,
        }
    }

    fn learn_command_end(&mut self, task_id: &str, end: &CommandEnd) {
        if !self.command_ends.learn(task_id, end) {
            return;
        }
        let rows: Vec<usize> = (0..self.transcript.messages.len())
            .filter(|&index| {
                self.command_row_task(index).is_some_and(|(id, row_knows)| {
                    id == task_id && row_knows.is_none_or(|known| known < end.source)
                })
            })
            .collect();
        for index in rows {
            self.write_command_end(index, end);
        }
    }

    fn write_command_end(&mut self, index: usize, end: &CommandEnd) {
        self.mutate_message(index, |message| {
            if let ChatMessage::ToolCall {
                output: Some(output),
                ..
            } = message
                && let Some(updated) = with_end(output, end)
            {
                *output = updated;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use orca_core::task_types::BackgroundTaskSummary;
    use serde_json::json;

    use crate::protocol::TuiEvent;
    use crate::surface_projection::SurfaceProjectionState;
    use crate::terminal_output::terminal_output_display;
    use crate::transcript_state::ChatMessage;
    use crate::types::AppState;

    fn state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    /// A shell result as the runtime writes it. `fields` override a call
    /// that returned while its command still ran.
    fn shell_result(task_id: &str, fields: serde_json::Value) -> String {
        let mut payload = json!({
            "task_id": task_id,
            "state": "running",
            "return_reason": "yield_elapsed",
            "termination_reason": null,
            "exit_code": null,
            "output": "4.0K\t.\n",
            "next_cursor": 6,
            "output_gap": null,
            "effective_deadline_ms": null,
            "deadline_source": null,
            "terminal": "pipe",
            "eof": false,
        });
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload.to_string()
    }

    fn running(task_id: &str) -> String {
        shell_result(task_id, json!({}))
    }

    fn ended(task_id: &str, state: &str, exit_code: i64) -> String {
        shell_result(
            task_id,
            json!({
                "state": state,
                "return_reason": "terminal_observed",
                "termination_reason": "exited",
                "exit_code": exit_code,
                "eof": true,
            }),
        )
    }

    /// Call `id` of `tool`, requested and completed with `output`.
    fn tool(state: &mut AppState, id: &str, tool: &str, output: String) {
        state.update(TuiEvent::ToolRequested {
            id: id.to_string(),
            name: tool.to_string(),
            target: None,
        });
        state.update(TuiEvent::ToolCompleted {
            id: id.to_string(),
            name: tool.to_string(),
            status: "completed".to_string(),
            output,
            diff: None,
            kind: None,
        });
    }

    /// The note the row of call `id` shows after its tool name.
    fn note(state: &AppState, id: &str) -> Option<String> {
        let output = state
            .transcript
            .messages
            .iter()
            .find_map(|message| match message {
                ChatMessage::ToolCall {
                    id: row, output, ..
                } if row == id => Some(output.clone()),
                _ => None,
            })
            .expect("the row")
            .expect("an output");
        terminal_output_display(&output)
            .expect("a shell result")
            .note
    }

    fn shell_task(id: &str, status: &str) -> BackgroundTaskSummary {
        serde_json::from_value(json!({
            "id": id,
            "type": "shell",
            "status": status,
            "description": "du -sh .",
            "createdAtMs": 1,
        }))
        .expect("task summary")
    }

    /// A shell task as the runtime publishes the end of a command that
    /// outlived its call: `error` says how a failed one ended.
    fn ended_shell_task(id: &str, status: &str, error: Option<&str>) -> BackgroundTaskSummary {
        serde_json::from_value(json!({
            "id": id,
            "type": "shell",
            "status": status,
            "description": "du -sh .",
            "createdAtMs": 1,
            "error": error,
        }))
        .expect("task summary")
    }

    /// The projection of a surface with `tasks`.
    fn projection(tasks: Vec<BackgroundTaskSummary>) -> SurfaceProjectionState {
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(3),
            session_id: Some("restored-session".to_string()),
            title: "restored".to_string(),
            usage_revision: 0,
            usage: Default::default(),
            context_revision: 0,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: tasks,
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
            mcp_catalog: Default::default(),
        }
    }

    /// The main session, running in the background: a task list that has it
    /// makes the reducer drop the output of the session.
    fn backgrounded_main_session() -> BackgroundTaskSummary {
        serde_json::from_value(json!({
            "id": "task-main",
            "type": "main_session",
            "status": "running",
            "isBackgrounded": true,
            "description": "long answer",
            "createdAtMs": 1,
        }))
        .expect("task summary")
    }

    fn restored(id: &str, tool: &str, output: String) -> ChatMessage {
        ChatMessage::ToolCall {
            id: id.to_string(),
            name: tool.to_string(),
            target: None,
            status: "completed".to_string(),
            output: Some(output),
            diff: None,
            kind: None,
            expanded: false,
        }
    }

    fn restore(state: &mut AppState, messages: Vec<ChatMessage>) {
        state.update(TuiEvent::HistoryLoaded {
            messages,
            plan: None,
            label: "Resumed saved conversation.".to_string(),
        });
    }

    #[test]
    fn a_task_list_showing_the_command_done_clears_still_running() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1",
            "completed",
        )]));
        assert_eq!(note(&state, "call-1"), None);
    }

    #[test]
    fn a_read_of_the_output_shows_the_exit_code() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        let rows = state.transcript.messages.len();
        tool(
            &mut state,
            "call-2",
            "task_read_output",
            ended("task-1", "failed", 2),
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("exit 2"));
        assert_eq!(
            state.transcript.messages.len(),
            rows,
            "the read itself is not shown"
        );
    }

    #[test]
    fn a_wait_settles_every_task_it_waited_on() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        tool(&mut state, "call-2", "bash", running("task-2"));
        tool(&mut state, "call-3", "task_wait", json!({
            "wait": "terminal",
            "return_reason": "terminal",
            "tasks": [
                {"session_id": "shell-1", "task_id": "task-1", "status": "completed",
                 "termination": "exited", "exit_code": 0, "output": "", "deadline_reached": false},
                {"session_id": "shell-2", "task_id": "task-2", "status": "stopped",
                 "termination": "timed_out", "exit_code": null, "output": "", "deadline_reached": true},
            ],
        }).to_string());
        assert_eq!(note(&state, "call-1"), None);
        assert_eq!(note(&state, "call-2").as_deref(), Some("timed out"));
    }

    #[test]
    fn rows_of_other_tasks_keep_their_state() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-9",
            "completed",
        )]));
        assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
    }

    #[test]
    fn a_command_the_task_list_still_runs_keeps_still_running() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1", "running",
        )]));
        assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
    }

    #[test]
    fn the_command_record_wins_over_the_task_list() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1", "failed",
        )]));
        assert_eq!(note(&state, "call-1").as_deref(), Some("failed"));
        tool(
            &mut state,
            "call-2",
            "task_read_output",
            ended("task-1", "failed", 3),
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("exit 3"));
        // Neither a later task list nor a stale read takes it back.
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1", "failed",
        )]));
        tool(&mut state, "call-3", "task_read_output", running("task-1"));
        assert_eq!(note(&state, "call-1").as_deref(), Some("exit 3"));
    }

    #[test]
    fn an_end_seen_before_the_row_arrives_is_shown_on_it() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1",
            "completed",
        )]));
        tool(&mut state, "call-1", "bash", running("task-1"));
        assert_eq!(note(&state, "call-1"), None);
    }

    #[test]
    fn restored_rows_show_how_their_commands_ended() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-3", "failed",
        )]));
        restore(
            &mut state,
            vec![
                restored("call-1", "bash", running("task-1")),
                restored(
                    "call-2",
                    "task_read_output",
                    ended("task-1", "completed", 0),
                ),
                restored("call-3", "bash", running("task-2")),
                restored("call-4", "bash", running("task-3")),
            ],
        );
        assert_eq!(note(&state, "call-1"), None);
        assert_eq!(note(&state, "call-3").as_deref(), Some("state unknown"));
        assert_eq!(note(&state, "call-4").as_deref(), Some("failed"));
    }

    #[test]
    fn a_task_list_after_the_restore_replaces_state_unknown() {
        let mut state = state();
        restore(
            &mut state,
            vec![restored("call-1", "bash", running("task-1"))],
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("state unknown"));
        state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task(
            "task-1",
            "completed",
        )]));
        assert_eq!(note(&state, "call-1"), None);
    }

    #[test]
    fn a_reattached_row_of_a_command_this_orca_saw_running_keeps_still_running() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        restore(
            &mut state,
            vec![restored("call-1", "bash", running("task-1"))],
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
    }

    #[test]
    fn a_reattached_row_shows_an_end_learned_before_the_reattach() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        tool(
            &mut state,
            "call-2",
            "task_read_output",
            ended("task-1", "failed", 4),
        );
        restore(
            &mut state,
            vec![restored("call-1", "bash", running("task-1"))],
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("exit 4"));
    }

    #[test]
    fn a_new_session_forgets_what_it_saw_running() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        state.update(TuiEvent::NewSessionStarted);
        restore(
            &mut state,
            vec![restored("call-1", "bash", running("task-1"))],
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("state unknown"));
    }

    #[test]
    fn only_the_commands_this_orca_saw_running_are_kept_running_on_a_reattach() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        restore(
            &mut state,
            vec![
                restored("call-1", "bash", running("task-1")),
                restored("call-2", "bash", running("task-2")),
            ],
        );
        assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
        assert_eq!(note(&state, "call-2").as_deref(), Some("state unknown"));
    }

    #[test]
    fn commands_seen_while_the_main_session_is_backgrounded_are_known_after_a_reattach() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated(vec![
            backgrounded_main_session(),
        ]));
        assert!(
            state.suppress_background_main_session_output,
            "the reducer drops the output of a backgrounded session"
        );
        let rows = state.transcript.messages.len();

        // The session starts two commands that outlive their calls, and reads
        // how the first one ended.
        tool(&mut state, "call-1", "bash", running("task-1"));
        tool(&mut state, "call-2", "bash", running("task-2"));
        tool(
            &mut state,
            "call-3",
            "task_read_output",
            ended("task-1", "completed", 0),
        );
        assert_eq!(
            state.transcript.messages.len(),
            rows,
            "nothing of it is shown while it is backgrounded"
        );

        // Attaching to the session brings its conversation back from history.
        state.update(TuiEvent::BackgroundTaskOutputAttached {
            task_id: "task-main".to_string(),
        });
        assert!(!state.suppress_background_main_session_output);
        restore(
            &mut state,
            vec![
                restored("call-1", "bash", running("task-1")),
                restored("call-2", "bash", running("task-2")),
            ],
        );
        assert_eq!(note(&state, "call-1"), None);
        assert_eq!(note(&state, "call-2").as_deref(), Some("still running"));
    }

    #[test]
    fn shell_tasks_say_how_their_commands_ended_and_stay_out_of_the_task_panel() {
        let mut state = state();
        tool(&mut state, "call-1", "bash", running("task-1"));
        tool(&mut state, "call-2", "bash", running("task-2"));
        let workflow: BackgroundTaskSummary = serde_json::from_value(json!({
            "id": "workflow-1",
            "type": "workflow",
            "status": "running",
            "description": "review",
            "createdAtMs": 1,
        }))
        .expect("task summary");
        state.update(TuiEvent::WorkflowTasksUpdated(vec![
            ended_shell_task("task-1", "failed", Some("exit code 3")),
            workflow,
            ended_shell_task("task-2", "failed", Some("timed out")),
        ]));
        assert_eq!(note(&state, "call-1").as_deref(), Some("exit 3"));
        assert_eq!(note(&state, "call-2").as_deref(), Some("timed out"));
        let ids = |tasks: &[BackgroundTaskSummary]| {
            tasks.iter().map(|task| task.id.clone()).collect::<Vec<_>>()
        };
        assert_eq!(ids(state.workflow_tasks()), ["workflow-1"]);
        assert_eq!(ids(state.command_tasks()), ["task-1", "task-2"]);
    }

    #[test]
    fn a_restored_row_shows_how_its_command_ended_in_either_restore_order() {
        let ended = || vec![ended_shell_task("task-1", "failed", Some("exit code 3"))];
        let rows = || vec![restored("call-1", "bash", running("task-1"))];

        // A resumed conversation: its history first, then the surface.
        let mut resumed = state();
        restore(&mut resumed, rows());
        resumed.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            ended(),
        ))));
        assert_eq!(note(&resumed, "call-1").as_deref(), Some("exit 3"));

        // A new hosted session: the surface, the session, then its history.
        let mut started = state();
        started.update(TuiEvent::SessionProjectionReset(Box::new(projection(
            ended(),
        ))));
        started.update(TuiEvent::NewSessionStarted);
        restore(&mut started, rows());
        assert_eq!(note(&started, "call-1").as_deref(), Some("exit 3"));
        assert!(started.workflow_tasks().is_empty());
    }

    /// The note of the latest bash row; `None` before there is one.
    #[cfg(unix)]
    fn latest_bash_note(state: &AppState) -> Option<Option<String>> {
        state
            .transcript
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                ChatMessage::ToolCall {
                    name,
                    output: Some(output),
                    ..
                } if name == "bash" => terminal_output_display(output).map(|display| display.note),
                _ => None,
            })
    }

    /// The task of the latest bash row; `None` before there is one.
    #[cfg(unix)]
    fn latest_bash_task(state: &AppState) -> Option<String> {
        state
            .transcript
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                ChatMessage::ToolCall {
                    name,
                    output: Some(output),
                    ..
                } if name == "bash" => {
                    crate::terminal_output::reported_task(output).map(|(task_id, _)| task_id)
                }
                _ => None,
            })
    }

    /// The row follows its command's end with nothing else going on: the
    /// runtime publishes the end on the surface as the command's shell task,
    /// which the idle TUI's poll brings in.
    #[cfg(unix)]
    #[test]
    fn a_bash_row_shows_how_its_command_ended_after_outliving_the_call() {
        use crate::test_support::hosted_tui::{Tui, config};

        let home = crate::test_support::isolate_orca_home();
        let mut config = config(home.path(), Vec::new());
        config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
        let mut tui = Tui::start(config);
        tui.send("bash_wait 200 :: sleep 2; echo done");
        tui.until("the call to return while its command runs", |state| {
            latest_bash_note(state) == Some(Some("still running".to_string()))
        });
        tui.until("the row to show its command ended", |state| {
            latest_bash_note(state) == Some(None)
        });
        tui.quit();
    }

    /// The same for a command that fails: the row shows its exit code.
    #[cfg(unix)]
    #[test]
    fn a_bash_row_shows_the_exit_code_of_a_command_that_outlived_the_call() {
        use crate::test_support::hosted_tui::{Tui, config};

        let home = crate::test_support::isolate_orca_home();
        let mut config = config(home.path(), Vec::new());
        config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
        let mut tui = Tui::start(config);
        tui.send("bash_wait 200 :: sleep 1; exit 3");
        tui.until("the call to return while its command runs", |state| {
            latest_bash_note(state) == Some(Some("still running".to_string()))
        });
        tui.until("the row to show the command's exit code", |state| {
            latest_bash_note(state) == Some(Some("exit 3".to_string()))
        });
        tui.quit();
    }

    /// What does happen: the model reads the output of a command that outlived
    /// its call, which the transcript does not show, and the row shows how the
    /// command ended.
    #[cfg(unix)]
    #[test]
    fn a_bash_row_shows_how_its_command_ended_once_the_model_reads_its_output() {
        use crate::test_support::hosted_tui::{Tui, config};
        use crate::types::AppStatus;

        let home = crate::test_support::isolate_orca_home();
        let mut config = config(home.path(), Vec::new());
        config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
        let mut tui = Tui::start(config);
        tui.send("bash_wait 200 :: sleep 1; exit 3");
        tui.until("the call to return while its command runs", |state| {
            latest_bash_note(state) == Some(Some("still running".to_string()))
        });
        let task_id = latest_bash_task(&tui.state).expect("the command's task");
        // The command ends on its own; a read of its output says how, once it has.
        for _ in 0..50 {
            tui.until("the turn to end", |state| state.status == AppStatus::Idle);
            tui.send(&format!("task_read_output {task_id}"));
            tui.until_event("the read to return", |event| {
                matches!(event, TuiEvent::ToolCompleted { name, .. } if name == "task_read_output")
            });
            if latest_bash_note(&tui.state) != Some(Some("still running".to_string())) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(
            latest_bash_note(&tui.state),
            Some(Some("exit 3".to_string()))
        );
        tui.quit();
    }
}
