// How commands that outlived their calls ended, on their way from the terminal
// supervisors to the typed surface: this file owns the ends the actor holds
// (ShellTaskEnds) and the ThreadActor methods that publish them.
use super::*;

use super::thread_actor_generation::ActorCommitFailure;
use crate::task_view::ShellEndDetail;
use crate::terminal_service::{SHELL_TASK_END_CAPACITY, ShellTaskEnd, ShellTaskEndInbox};

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

/// What was said when the end of the command a test names was dropped.
#[cfg(test)]
static DROPPED_SHELL_TASK_ENDS: std::sync::OnceLock<Mutex<HashMap<String, String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn dropped_shell_task_end_message(description: &str) -> Option<String> {
    DROPPED_SHELL_TASK_ENDS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(description)
        .cloned()
}

/// The failure a test made this commit of the end of the command
/// `description` meet; never one outside tests.
#[cfg(not(test))]
fn shell_task_end_commit_failure(_description: &str) -> Option<ActorCommitFailure> {
    None
}

#[cfg(test)]
fn shell_task_end_commit_failure(description: &str) -> Option<ActorCommitFailure> {
    let mut failures = SHELL_TASK_END_COMMIT_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let count = failures.get_mut(description)?;
    if *count <= 1 {
        failures.remove(description);
    } else {
        *count -= 1;
    }
    Some(ActorCommitFailure::Commit(
        surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::AppendFailed),
    ))
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
        let left = self.inbox.take();
        self.keep(left);
    }

    /// Keeps `ends` to publish, at most `SHELL_TASK_END_CAPACITY` of them:
    /// past it the oldest is dropped, as the supervisor drops them.
    fn keep(&mut self, ends: Vec<ShellTaskEnd>) {
        for end in ends {
            if self.pending.len() >= SHELL_TASK_END_CAPACITY {
                self.pending.pop_front();
            }
            self.pending.push_back(PendingShellTaskEnd {
                end,
                failed_commits: 0,
            });
        }
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
            let batch = self.surface_event_batch_with_commit_id(events, None);
            let committed = match shell_task_end_commit_failure(&pending.end.description) {
                Some(failure) => Err(failure),
                None => self.commit_surface_actor_batch_naming_failure(&batch),
            };
            match committed {
                Ok(()) => {
                    self.shell_task_ends.pending.pop_front();
                }
                Err(failure) => {
                    let pending = self
                        .shell_task_ends
                        .pending
                        .front_mut()
                        .expect("the end that failed to commit is pending");
                    pending.failed_commits = pending.failed_commits.saturating_add(1);
                    if pending.failed_commits >= SHELL_TASK_END_COMMIT_ATTEMPTS {
                        let message = format!(
                            "orca: dropped how command task {} ended: its commit failed {} times, \
                             the last because of {failure}",
                            pending.end.task_id, pending.failed_commits
                        );
                        eprintln!("{message}");
                        #[cfg(test)]
                        DROPPED_SHELL_TASK_ENDS
                            .get_or_init(|| Mutex::new(HashMap::new()))
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(pending.end.description.clone(), message);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(index: usize) -> ShellTaskEnd {
        ShellTaskEnd {
            task_id: format!("task-{index}"),
            description: "true".to_string(),
            started_at_ms: 1,
            ended_at_ms: 2,
            status: TaskStatus::Completed,
            exit_code: Some(0),
            deadline_reached: false,
        }
    }

    #[test]
    fn the_actor_keeps_the_newest_ends_up_to_the_supervisors_capacity() {
        let (_sink, inbox) = crate::terminal_service::ShellTaskEndSink::channel();
        let mut ends = ShellTaskEnds::new(inbox);
        let overflow = 44;
        ends.keep((0..200).map(ended).collect());
        ends.keep(
            (200..SHELL_TASK_END_CAPACITY + overflow)
                .map(ended)
                .collect(),
        );

        assert_eq!(ends.pending.len(), SHELL_TASK_END_CAPACITY);
        assert_eq!(
            ends.pending
                .front()
                .map(|pending| pending.end.task_id.as_str()),
            Some(format!("task-{overflow}").as_str())
        );
        assert_eq!(
            ends.pending
                .back()
                .map(|pending| pending.end.task_id.as_str()),
            Some(format!("task-{}", SHELL_TASK_END_CAPACITY + overflow - 1).as_str())
        );
    }
}
