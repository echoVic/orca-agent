// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

use crate::task_view::ShellEndDetail;
use crate::terminal_service::{ShellTaskEnd, ShellTaskEndInbox};

/// Polls on which one end may fail to commit before it is dropped.
pub(super) const SHELL_TASK_END_COMMIT_ATTEMPTS: u8 = 20;

/// Commit failures a test makes the end of the command it names meet.
#[cfg(test)]
static SHELL_TASK_END_COMMIT_FAILURES: std::sync::OnceLock<Mutex<HashMap<String, u8>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn inject_shell_task_end_commit_failures(description: &str, count: u8) {
    SHELL_TASK_END_COMMIT_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(description.to_string(), count);
}

#[cfg(test)]
fn take_shell_task_end_commit_failure(description: &str) -> bool {
    let mut failures = SHELL_TASK_END_COMMIT_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(count) = failures.get_mut(description) else {
        return false;
    };
    if *count <= 1 {
        failures.remove(description);
    } else {
        *count -= 1;
    }
    true
}

/// How the thread's commands that outlived their calls ended: what its
/// terminal supervisors left, and what the typed surface does not show yet.
pub(super) struct ShellTaskEnds {
    inbox: ShellTaskEndInbox,
    pending: VecDeque<PendingShellTaskEnd>,
}

struct PendingShellTaskEnd {
    end: ShellTaskEnd,
    failed_commits: u8,
}

impl ShellTaskEnds {
    pub(super) fn new(inbox: ShellTaskEndInbox) -> Self {
        Self {
            inbox,
            pending: VecDeque::new(),
        }
    }

    /// Waits until a supervisor leaves an end; `None` once none can.
    pub(super) async fn wake(&mut self) -> Option<()> {
        self.inbox.wake().await
    }

    pub(super) fn is_closed(&self) -> bool {
        self.inbox.is_closed()
    }

    /// Takes what the supervisors left, to publish once the surface may
    /// commit.
    pub(super) fn take_left(&mut self) {
        self.pending.extend(
            self.inbox
                .take()
                .into_iter()
                .map(|end| PendingShellTaskEnd {
                    end,
                    failed_commits: 0,
                }),
        );
    }
}

impl ThreadActor {
    /// Publishes, oldest first, how the commands that outlived their calls
    /// ended, each as its Shell task created and settled in one batch. An
    /// end whose task the surface already has was published before. When a
    /// commit fails the end waits for the next poll, and the ends behind it
    /// with it; one that keeps failing is dropped.
    pub(super) fn publish_shell_task_ends(&mut self) {
        if self.shell_task_ends.pending.is_empty() {
            return;
        }
        if self.resident_surface.0.is_none() {
            // Without a typed surface there is nothing to show them on.
            self.shell_task_ends.pending.clear();
            return;
        }
        while let Some(pending) = self.shell_task_ends.pending.front() {
            let published = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .tasks
                .iter()
                .any(|task| task.task_id.as_str() == pending.end.task_id);
            let Some(events) = (!published)
                .then(|| shell_task_settled_events(&pending.end))
                .flatten()
            else {
                self.shell_task_ends.pending.pop_front();
                continue;
            };
            #[cfg(test)]
            let injected_failure = take_shell_task_end_commit_failure(&pending.end.description);
            #[cfg(not(test))]
            let injected_failure = false;
            let batch = self.surface_event_batch_with_commit_id(events, None);
            let committed = if injected_failure {
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            } else {
                self.commit_surface_actor_batch_with_retry(&batch)
            };
            match committed {
                Ok(()) => {
                    self.shell_task_ends.pending.pop_front();
                }
                Err(error) => {
                    let pending = self
                        .shell_task_ends
                        .pending
                        .front_mut()
                        .expect("the end that failed to commit is pending");
                    pending.failed_commits = pending.failed_commits.saturating_add(1);
                    if pending.failed_commits >= SHELL_TASK_END_COMMIT_ATTEMPTS {
                        eprintln!(
                            "orca: gave up showing how command task {} ended: {error:?}",
                            pending.end.task_id
                        );
                        self.shell_task_ends.pending.pop_front();
                    }
                    return;
                }
            }
        }
    }
}

/// The batch that shows how `end`'s command ended: its task created running,
/// as its call left it, and settled. `None` for an end no task can show.
///
/// | The supervisor saw      | Status    | Error         |
/// |-------------------------|-----------|---------------|
/// | exit 0                  | Completed | -             |
/// | exit N                  | Failed    | `exit code N` |
/// | no exit code            | Failed    | -             |
/// | its deadline stopped it | Failed    | `timed out`   |
/// | a stop or a kill        | Stopped   | -             |
///
/// The commit authority accepts exactly this shape from the actor
/// (`actor_control_shell_task_settled_authorized`).
fn shell_task_settled_events(
    end: &ShellTaskEnd,
) -> Option<Vec<(surface::SurfaceScope, surface::SurfaceEvent)>> {
    let (status, detail) = if end.deadline_reached {
        (
            surface::SurfaceTaskStatus::Failed,
            Some(ShellEndDetail::TimedOut),
        )
    } else {
        match end.status {
            TaskStatus::Completed => (surface::SurfaceTaskStatus::Completed, None),
            TaskStatus::Failed => (
                surface::SurfaceTaskStatus::Failed,
                end.exit_code.map(ShellEndDetail::ExitCode),
            ),
            TaskStatus::Stopped => (surface::SurfaceTaskStatus::Stopped, None),
            TaskStatus::Cancelled => (surface::SurfaceTaskStatus::Cancelled, None),
            TaskStatus::Queued
            | TaskStatus::Running
            | TaskStatus::Paused
            | TaskStatus::Stopping
            | TaskStatus::ApprovalRequired => return None,
        }
    };
    let task_id = surface::SurfaceTaskId::try_new(end.task_id.clone()).ok()?;
    let started_at = surface::UnixMillis::new(end.started_at_ms);
    let created = surface::SurfaceTask {
        task_id: task_id.clone(),
        revision: surface::TaskRevision::try_new(1).expect("one is a valid task revision"),
        task_type: surface::SurfaceTaskType::Shell,
        status: surface::SurfaceTaskStatus::Running,
        backgrounded: false,
        description: surface::DisplayText::new(end.description.clone()),
        created_at: started_at,
        started_at: Some(started_at),
        completed_at: None,
        parent_operation: None,
        parent_task_id: None,
        background_fence: None,
        workflow_run_id: None,
        subagent_id: None,
        pending_interaction_id: None,
        usage: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
    };
    Some(vec![
        (
            surface::SurfaceScope::Thread,
            surface::SurfaceEvent::Task(surface::TaskPatch::Upserted {
                expected_revision: None,
                task: created,
            }),
        ),
        (
            surface::SurfaceScope::Thread,
            surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                task_id,
                expected_revision: surface::TaskRevision::try_new(1)
                    .expect("one is a valid task revision"),
                next_revision: surface::TaskRevision::try_new(2)
                    .expect("two is a valid task revision"),
                status,
                completed_at: Some(surface::UnixMillis::new(end.ended_at_ms)),
                result: None,
                error: detail.map(|detail| surface::DisplayText::new(detail.to_string())),
            }),
        ),
    ])
}
