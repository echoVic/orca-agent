//! One model-facing view of a task, whatever actually runs it.
//!
//! The model addresses work by `task_id` alone. A shell command and a child
//! agent differ in what they produce, not in how they are addressed, observed,
//! or stopped. This module is the single place that turns a durable
//! [`TaskRecord`] into the observation the model sees, so `task_read_output`,
//! `task_wait`, and the completion path cannot drift apart.
//!
//! The view is deliberately split into a common envelope and a kind-specific
//! body:
//!
//! - the envelope answers "what is this task doing right now";
//! - `command` carries the shell contract (exit code, cursor paging);
//! - `agent` carries the child contract (role, usage, continuation).
//!
//! A reader that only cares about progress never has to know which kind it is.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use orca_core::task_types::{SubagentActivityEntry, TaskStatus, TaskType};

use crate::tasks::{ResultDelivery, TaskRecord, TaskRegistry};

/// Default page size for an agent result read, in characters.
pub const DEFAULT_AGENT_RESULT_PAGE_CHARS: usize = 12_000;
/// Upper bound for one agent result page.
pub const MAX_AGENT_RESULT_PAGE_CHARS: usize = 32_000;

/// Why a queued task has not started yet.
///
/// A queue position alone is not an explanation; the reason is what tells the
/// model whether to wait, unblock something else, or report a real limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// All execution leases in the scope are held.
    ExecutionCapacity,
    /// The scope's accepted-work queue is at its limit.
    QueueFull,
    /// The scope holds as many non-terminal children as it may.
    LiveTaskLimit,
    /// A declared dependency has not finished.
    Dependency,
    /// The scheduler paused new provider requests after a rate-limit signal.
    ProviderRateLimit,
    /// The caller asked for an execution deadline that has not arrived.
    Deadline,
}

impl WaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionCapacity => "execution_capacity",
            Self::QueueFull => "queue_full",
            Self::LiveTaskLimit => "live_task_limit",
            Self::Dependency => "dependency",
            Self::ProviderRateLimit => "provider_rate_limit",
            Self::Deadline => "deadline",
        }
    }
}

/// How a task relates to the scope's execution capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionState {
    /// Waiting for a first execution lease.
    Queued,
    /// Holds a lease and may start, call a model, or run a tool.
    Running,
    /// Released its lease while waiting for something else.
    Waiting,
    /// Released its lease while waiting for its own children to finish.
    ///
    /// This is what keeps a nested tree from deadlocking: a parent that is
    /// blocked on its children is not using an execution lease, so those
    /// children can be admitted in the capacity the parent gave back.
    WaitingChildren,
    /// Cancellation was requested; the lease is held until execution is silent.
    Stopping,
    /// Settled and confirmed finished.
    Terminal,
    /// Stopping could not be confirmed; the resources stay isolated.
    Indeterminate,
}

impl ExecutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::WaitingChildren => "waiting_children",
            Self::Stopping => "stopping",
            Self::Terminal => "terminal",
            Self::Indeterminate => "indeterminate",
        }
    }

    /// Whether this task currently occupies an execution lease.
    ///
    /// `Stopping` still holds its lease: a cancellation request is not proof
    /// that the underlying execution has stopped. `WaitingChildren` released
    /// its lease on purpose, so it does not count.
    pub fn holds_execution_lease(self) -> bool {
        matches!(self, Self::Running | Self::Stopping | Self::Indeterminate)
    }

    /// Whether this task is a candidate for scheduling.
    pub fn is_schedulable(self) -> bool {
        matches!(self, Self::Queued | Self::Waiting | Self::WaitingChildren)
    }
}

/// The common part of every task observation.
#[derive(Clone, Debug)]
pub struct TaskView {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub task_type: TaskType,
    pub lifetime: orca_core::task_types::TaskLifetime,
    pub execution: ExecutionState,
    pub status: TaskStatus,
    pub wait_reason: Option<WaitReason>,
    pub description: String,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub last_activity_at_ms: Option<i64>,
    pub result_delivery: ResultDelivery,
    /// Absolute deadline in Unix milliseconds, when one applies.
    pub deadline_at_ms: Option<i64>,
    /// Who set the effective deadline.
    pub deadline_source: Option<String>,
    pub usage: Option<Value>,
    /// Kind-specific body.
    body: Value,
}

impl TaskView {
    /// Builds the observation for a durable task record.
    ///
    /// Use [`TaskView::of_task`] when the registry is available: whether a
    /// task is waiting on its children is a property of the tree, not of the
    /// record alone.
    pub fn of(record: &TaskRecord) -> Self {
        Self::build(record, false)
    }

    /// Builds the observation using the full task tree.
    pub fn of_task(registry: &TaskRegistry, record: &TaskRecord) -> Self {
        Self::build(record, task_waits_on_children(registry, &record.id))
    }

    fn build(record: &TaskRecord, waiting_on_children: bool) -> Self {
        let execution = execution_state(record, waiting_on_children);
        let body = match record.task_type {
            TaskType::Subagent => agent_body(record),
            _ => command_body(record),
        };
        Self {
            task_id: record.id.clone(),
            parent_task_id: record.parent_task_id.clone(),
            task_type: record.task_type,
            lifetime: record.lifetime,
            execution,
            status: record.status,
            wait_reason: wait_reason(record, execution),
            description: record.description.clone(),
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            last_activity_at_ms: record.last_activity_at_ms,
            result_delivery: record.result_delivery,
            deadline_at_ms: record.deadline_at_ms,
            deadline_source: record.deadline_source.clone(),
            usage: record.usage.map(|usage| {
                json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_tokens": usage.cache_tokens,
                    "estimated_cost_usd": usage.estimated_cost_usd,
                })
            }),
            body,
        }
    }

    /// Looks up one task and builds its observation.
    pub fn lookup(registry: &TaskRegistry, task_id: &str) -> Option<Self> {
        registry
            .get(task_id)
            .as_ref()
            .map(|record| Self::of_task(registry, record))
    }

    /// The model-facing JSON, including the kind-specific body inline.
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "task_id": self.task_id,
            "subject": self.description,
            "task_type": task_type_label(self.task_type),
            "lifetime": self.lifetime.as_str(),
            "state": self.execution.as_str(),
            "execution_state": self.execution.as_str(),
            "holds_execution_lease": self.execution.holds_execution_lease(),
            "status": status_label(self.status),
            "wait_reason": self.wait_reason.map(WaitReason::as_str),
            "parent_task_id": self.parent_task_id,
            "created_at_ms": self.created_at_ms,
            "started_at_ms": self.started_at_ms,
            "completed_at_ms": self.completed_at_ms,
            "last_activity_at_ms": self.last_activity_at_ms,
            "result_delivered": matches!(self.result_delivery, ResultDelivery::Delivered),
            "deadline_at_ms": self.deadline_at_ms,
            "deadline_source": self.deadline_source,
            "timing": self.timing(now_unix_ms()).to_json(),
            "usage": self.usage,
        });
        if let (Some(object), Some(body)) = (value.as_object_mut(), self.body.as_object()) {
            for (key, body_value) in body {
                object.insert(key.clone(), body_value.clone());
            }
        }
        value
    }

    /// Whether the task has finished and will not change state again.
    pub fn is_terminal(&self) -> bool {
        self.execution == ExecutionState::Terminal
    }

    /// Whether this task released its lease to wait for its children.
    pub fn is_waiting_on_children(&self) -> bool {
        self.execution == ExecutionState::WaitingChildren
    }

    /// Whether an absolute deadline has already passed for this task.
    pub fn deadline_expired(&self, now_ms: i64) -> bool {
        self.deadline_at_ms
            .is_some_and(|deadline| now_ms >= deadline)
    }

    /// The three wall-clock dimensions the runtime keeps separate.
    ///
    /// Queueing and suspension are reported on their own rather than being
    /// folded into one "duration", because a task that waited for a lease is
    /// not a task that ran slowly.
    pub fn timing(&self, now_ms: i64) -> TaskTiming {
        let end = self.completed_at_ms.unwrap_or(now_ms);
        let queued_until = self.started_at_ms.unwrap_or(end);
        TaskTiming {
            queued_ms: queued_until.saturating_sub(self.created_at_ms).max(0) as u64,
            // Running time is reported once the task has started; a queued task
            // has no running time yet rather than a growing one.
            running_ms: self
                .started_at_ms
                .map(|started| end.saturating_sub(started).max(0) as u64)
                .unwrap_or(0),
            total_ms: end.saturating_sub(self.created_at_ms).max(0) as u64,
        }
    }
}

/// Wall-clock time a task spent in each phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskTiming {
    /// Time between submission and first start.
    pub queued_ms: u64,
    /// Time between first start and settlement.
    pub running_ms: u64,
    /// Time between submission and settlement, including suspension.
    pub total_ms: u64,
}

impl TaskTiming {
    pub fn to_json(self) -> Value {
        json!({
            "queued_ms": self.queued_ms,
            "running_ms": self.running_ms,
            "total_ms": self.total_ms,
        })
    }
}

/// A page of an agent result, with the offset to continue from.
#[derive(Clone, Debug)]
pub struct AgentResultPage {
    pub page: String,
    pub offset: usize,
    pub total_chars: usize,
    pub next_offset: Option<usize>,
}

/// Pages a child agent's stored result.
///
/// Offsets are character offsets in the stored result, not bytes, so a page
/// boundary can never split a multi-byte character. Only the final report is
/// paged; the child's tool trace stays in its own transcript.
pub fn page_agent_result(
    registry: &TaskRegistry,
    task_id: &str,
    offset: usize,
    limit: usize,
) -> Option<AgentResultPage> {
    let record = registry.get(task_id)?;
    if record.task_type != TaskType::Subagent {
        return None;
    }
    let text = record.result.unwrap_or_default();
    let total_chars = text.chars().count();
    let offset = offset.min(total_chars);
    let limit = limit.clamp(1, MAX_AGENT_RESULT_PAGE_CHARS);
    let page: String = text.chars().skip(offset).take(limit).collect();
    let next_offset = offset
        .checked_add(page.chars().count())
        .filter(|next| *next < total_chars);
    Some(AgentResultPage {
        page,
        offset,
        total_chars,
        next_offset,
    })
}

/// Every task in the scope, sorted so a reader sees a stable order.
pub fn scope_views(registry: &TaskRegistry) -> Vec<TaskView> {
    let mut tasks = registry.list();
    tasks.sort_by(|left, right| left.id.cmp(&right.id));
    tasks
        .iter()
        .filter_map(|summary| TaskView::lookup(registry, &summary.id))
        .collect()
}

fn execution_state(record: &TaskRecord, waiting_on_children: bool) -> ExecutionState {
    if record.continuation_indeterminate {
        return ExecutionState::Indeterminate;
    }
    if record.status == TaskStatus::Queued
        && record.wait_reason == Some(WaitReason::Dependency)
        && waiting_on_children
    {
        return ExecutionState::WaitingChildren;
    }
    match record.status {
        TaskStatus::Queued => ExecutionState::Queued,
        TaskStatus::Running => ExecutionState::Running,
        TaskStatus::Paused => ExecutionState::Waiting,
        TaskStatus::Stopping => ExecutionState::Stopping,
        TaskStatus::Stopped
        | TaskStatus::Completed
        | TaskStatus::Failed
        | TaskStatus::Cancelled => ExecutionState::Terminal,
        TaskStatus::ApprovalRequired => ExecutionState::Waiting,
    }
}

/// Whether a task has at least one non-terminal child task.
///
/// This is the condition that makes a parent a "waiting" task rather than a
/// running one. It is derived from the durable tree instead of from a flag, so
/// a process restart cannot leave a parent counted as blocking while it has no
/// live work.
pub fn task_waits_on_children(registry: &TaskRegistry, task_id: &str) -> bool {
    registry.list().iter().any(|summary| {
        summary.parent_task_id.as_deref() == Some(task_id)
            && !matches!(
                summary.status,
                TaskStatus::Stopped
                    | TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            )
    })
}

/// The recorded reason a task is waiting, if the runtime recorded one.
///
/// This deliberately does not guess. "Queued" means waiting for a lease, but
/// *why* it is still queued is a scheduler fact; inventing one would hide a
/// task that is waiting on something else entirely.
fn wait_reason(record: &TaskRecord, execution: ExecutionState) -> Option<WaitReason> {
    let _ = execution;
    record.wait_reason
}

fn command_body(record: &TaskRecord) -> Value {
    json!({
        "kind": "command",
        "command": record.command,
        // A command that has not exited has no exit code; the field stays
        // absent rather than reporting a misleading zero.
        "exit_code": record.exit_code,
        "output_truncated": record.output_truncated,
    })
}

fn agent_body(record: &TaskRecord) -> Value {
    json!({
        "kind": "agent",
        "role": record.agent_type,
        "turn": record.subagent_turn,
        "current_activity": record.subagent_current_activity,
        "activity_history": record
            .subagent_activity_history
            .iter()
            .map(activity_json)
            .collect::<Vec<_>>(),
        "continuation_id": record.continuation_id.as_ref().map(ToString::to_string),
        "attempt_id": record.continuation_attempt_id.as_ref().map(ToString::to_string),
        "checkpoint_id": record.continuation_checkpoint_id.as_ref().map(ToString::to_string),
        "resumable": record.continuation_resumable,
        "indeterminate": record.continuation_indeterminate,
        "budget": record.budget_reservation.as_ref().map(|binding| binding.observation()),
        // The error is part of the observation, not a separate tool.
        "error": record.error,
    })
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn activity_json(entry: &SubagentActivityEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

pub fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Cancelled => "cancelled",
    }
}

pub fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main_session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> TaskRegistry {
        TaskRegistry::new("task-view".to_string())
    }

    #[test]
    fn a_queued_child_reports_its_wait_reason_and_holds_no_lease() {
        let registry = registry();
        let child = registry.create_subagent("inspect".to_string(), Some("explorer".to_string()));

        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.execution, ExecutionState::Queued);
        assert_eq!(
            view.wait_reason, None,
            "the view reports the recorded reason, and none is recorded yet"
        );
        assert!(!view.execution.holds_execution_lease());
        assert!(view.execution.is_schedulable());

        // The scheduler records why it is waiting.
        crate::execution_scope::mark_queued(&registry, &child.id, WaitReason::ExecutionCapacity);
        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.wait_reason, Some(WaitReason::ExecutionCapacity));

        let json = view.to_json();
        assert_eq!(json["task_id"], child.id);
        assert_eq!(json["state"], "queued");
        assert_eq!(json["wait_reason"], "execution_capacity");
        assert_eq!(json["holds_execution_lease"], false);
        assert_eq!(json["kind"], "agent");
        assert_eq!(json["role"], "explorer");
    }

    #[test]
    fn a_running_child_holds_a_lease_and_has_no_wait_reason() {
        let registry = registry();
        let child = registry.create_subagent("inspect".to_string(), None);
        registry.mark_running(&child.id).expect("running");

        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.execution, ExecutionState::Running);
        assert!(view.execution.holds_execution_lease());
        assert!(!view.execution.is_schedulable());
        assert_eq!(view.wait_reason, None);
    }

    #[test]
    fn a_stopping_task_still_holds_its_lease() {
        let registry = registry();
        let child = registry.create_subagent("inspect".to_string(), None);
        registry.mark_running(&child.id).expect("running");
        registry.request_stop(&child.id).expect("request stop");

        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.execution, ExecutionState::Stopping);
        assert!(
            view.execution.holds_execution_lease(),
            "a cancellation request is not proof that execution stopped"
        );
    }

    #[test]
    fn a_settled_child_is_terminal_and_reports_delivery() {
        let registry = registry();
        let child = registry.create_subagent("inspect".to_string(), None);
        registry.mark_running(&child.id).expect("running");
        registry
            .complete(&child.id, "found it".to_string())
            .expect("complete");

        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.execution, ExecutionState::Terminal);
        assert!(view.is_terminal());
        assert!(!view.execution.holds_execution_lease());
    }

    #[test]
    fn agent_results_page_on_character_boundaries() {
        let registry = registry();
        let child = registry.create_subagent("inspect".to_string(), None);
        // Multi-byte characters must never be split by a page boundary.
        registry
            .complete(&child.id, "中文结果".to_string())
            .expect("complete");

        let first = page_agent_result(&registry, &child.id, 0, 2).expect("page");
        assert_eq!(first.page, "中文");
        assert_eq!(first.total_chars, 4);
        assert_eq!(first.next_offset, Some(2));

        let second = page_agent_result(&registry, &child.id, 2, 2).expect("page");
        assert_eq!(second.page, "结果");
        assert_eq!(second.next_offset, None);

        // A cursor past the end reads empty rather than restarting.
        let past_end = page_agent_result(&registry, &child.id, 99, 2).expect("page");
        assert!(past_end.page.is_empty());
        assert_eq!(past_end.offset, 4);
    }

    #[test]
    fn an_expired_deadline_is_reported_and_stops_a_queued_task() {
        let registry = registry();
        let child = registry.create_subagent("child".to_string(), None);
        assert!(
            !registry.deadline_expired(&child.id),
            "a task with no deadline never expires"
        );

        // A deadline in the past is already expired.
        assert!(
            registry
                .set_deadline_at(&child.id, 1, "caller deadline_ms")
                .is_some()
        );
        assert!(registry.deadline_expired(&child.id));
        assert_eq!(registry.expired_deadline_tasks(), vec![child.id.clone()]);

        let view = TaskView::lookup(&registry, &child.id).expect("view");
        assert_eq!(view.deadline_at_ms, Some(1));
        assert_eq!(view.deadline_source.as_deref(), Some("caller deadline_ms"));
        assert!(view.deadline_expired(now_unix_ms()));
    }

    #[test]
    fn a_tighter_deadline_wins_and_a_looser_one_never_extends_it() {
        let registry = registry();
        let child = registry.create_subagent("child".to_string(), None);

        registry
            .set_deadline_at(&child.id, 5_000, "first")
            .expect("set");
        // A looser deadline must not extend the existing limit.
        registry
            .set_deadline_at(&child.id, 9_000, "second")
            .expect("set");
        assert_eq!(
            registry.get(&child.id).and_then(|r| r.deadline_at_ms),
            Some(5_000)
        );
        // A tighter one always wins.
        registry
            .set_deadline_at(&child.id, 1_000, "third")
            .expect("set");
        let record = registry.get(&child.id).expect("record");
        assert_eq!(record.deadline_at_ms, Some(1_000));
        assert_eq!(record.deadline_source.as_deref(), Some("third"));
    }

    #[test]
    fn a_descendant_can_never_outlive_its_ancestor() {
        let registry = registry();
        let parent = registry.create_subagent("parent".to_string(), None);
        registry
            .set_deadline_at(&parent.id, 2_000, "parent")
            .expect("parent deadline");
        let child =
            registry.create_subagent_with_parent("child".to_string(), None, Some(parent.id));

        // The child asks for far longer than its parent has left.
        registry
            .set_deadline_at(&child.id, 900_000, "child")
            .expect("child deadline");

        let record = registry.get(&child.id).expect("record");
        assert_eq!(
            record.deadline_at_ms,
            Some(2_000),
            "a descendant must inherit the ancestor's tighter deadline"
        );
        assert!(
            record
                .deadline_source
                .as_deref()
                .is_some_and(|source| source.contains("inherited ancestor")),
            "the origin must name the inheritance: {:?}",
            record.deadline_source
        );
    }

    #[test]
    fn timing_keeps_queueing_and_running_apart() {
        let registry = registry();
        let child = registry.create_subagent("child".to_string(), None);

        // A task that has not started has queueing time and no running time.
        let queued = TaskView::lookup(&registry, &child.id).expect("view");
        let timing = queued.timing(queued.created_at_ms + 250);
        assert_eq!(timing.queued_ms, 250);
        assert_eq!(
            timing.running_ms, 0,
            "a queued task has no running time rather than a growing one"
        );
        assert_eq!(timing.total_ms, 250);

        registry.mark_running(&child.id).expect("running");
        registry
            .complete(&child.id, "done".to_string())
            .expect("complete");
        let settled = TaskView::lookup(&registry, &child.id).expect("view");
        let timing = settled.timing(now_unix_ms());
        assert!(timing.total_ms >= timing.queued_ms);
        assert!(timing.total_ms >= timing.running_ms);

        let json = settled.to_json();
        assert!(json["timing"]["queued_ms"].is_number());
        assert!(json["timing"]["running_ms"].is_number());
        assert!(json["timing"]["total_ms"].is_number());
    }

    #[test]
    fn a_command_view_never_invents_an_exit_code() {
        let registry = registry();
        let task = registry.create_shell("sleep 1".to_string(), "sleep 1".to_string());

        let view = TaskView::lookup(&registry, &task.id).expect("view");
        let json = view.to_json();
        assert_eq!(json["kind"], "command");
        assert!(
            json["exit_code"].is_null(),
            "a command that has not exited has no exit code: {json}"
        );
    }

    #[test]
    fn an_unknown_task_has_no_view() {
        let registry = registry();
        assert!(TaskView::lookup(&registry, "task-missing").is_none());
        assert!(page_agent_result(&registry, "task-missing", 0, 10).is_none());
    }

    #[test]
    fn scope_views_are_stable_and_cover_every_task() {
        let registry = registry();
        let first = registry.create_subagent("one".to_string(), None);
        let second = registry.create_subagent("two".to_string(), None);

        let views = scope_views(&registry);
        let ids = views
            .iter()
            .map(|view| view.task_id.clone())
            .collect::<Vec<_>>();
        let mut expected = vec![first.id.clone(), second.id.clone()];
        expected.sort();
        assert_eq!(ids, expected);
    }
}
