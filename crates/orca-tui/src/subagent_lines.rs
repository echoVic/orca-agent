use std::collections::{HashMap, HashSet};

use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_runtime::surface::{
    SurfaceItem, SurfaceSnapshot, SurfaceSubagent, SurfaceSubagentOwner, SurfaceTask,
    SurfaceTaskType,
};

pub(crate) fn delegating_line(batch_size: usize) -> String {
    if batch_size <= 1 {
        "Delegating to 1 agent".into()
    } else {
        format!("Delegating to {batch_size} agents in parallel")
    }
}

pub(crate) fn agent_ended_line(
    status: TaskStatus,
    name: &str,
    error: Option<&str>,
) -> Option<String> {
    Some(match status {
        TaskStatus::Completed => format!("Agent completed: {name}"),
        TaskStatus::Failed => format!("Agent failed: {}", error.unwrap_or(name)),
        TaskStatus::Blocked => format!("Agent blocked: {}", error.unwrap_or(name)),
        TaskStatus::Cancelled => format!("Agent cancelled: {name}"),
        TaskStatus::Stopped => format!("Agent stopped: {name}"),
        _ => return None,
    })
}

fn owner<'a>(
    task: &'a SurfaceTask,
    agent: Option<&'a SurfaceSubagent>,
) -> Option<&'a orca_runtime::surface::SurfaceOperationId> {
    task.parent_operation
        .as_ref()
        .or_else(|| match &agent?.owner {
            SurfaceSubagentOwner::Generation { fence } => Some(&fence.operation_id),
            SurfaceSubagentOwner::DetachedTask { .. } => None,
        })
}

pub(crate) fn batch_identity(task: &SurfaceTask, agent: &SurfaceSubagent) -> String {
    let id = agent.batch_id.as_str();
    match owner(task, Some(agent)) {
        Some(operation) => format!("{}:{id}", uuid::Uuid::from_bytes(*operation.as_bytes())),
        None => id.to_owned(),
    }
}

pub(crate) fn batch_key(task: &BackgroundTaskSummary) -> &str {
    task.subagent_batch_id.as_deref().unwrap_or(&task.id)
}

pub(crate) fn ended(task: &BackgroundTaskSummary) -> Option<String> {
    agent_ended_line(
        task.status,
        task.name.as_deref().unwrap_or(&task.description),
        task.error.as_deref(),
    )
}

#[derive(Debug, Clone, Default)]
pub struct AnnouncedAgentLines {
    pub(crate) batches: HashSet<String>,
    pub(crate) terminals: HashSet<String>,
}

impl AnnouncedAgentLines {
    pub(crate) fn from_tasks(tasks: &[BackgroundTaskSummary]) -> Self {
        let agents = tasks
            .iter()
            .filter(|task| task.task_type == TaskType::Subagent);
        Self {
            batches: agents
                .clone()
                .map(|task| batch_key(task).to_owned())
                .collect(),
            terminals: agents
                .filter(|task| ended(task).is_some())
                .map(|task| task.id.clone())
                .collect(),
        }
    }
}

pub(crate) fn positioned_lines(snapshot: &SurfaceSnapshot) -> Vec<(usize, String)> {
    if !snapshot
        .tasks
        .iter()
        .any(|task| task.task_type == SurfaceTaskType::Subagent)
    {
        return Vec::new();
    }
    let main_starts: HashMap<_, _> = snapshot
        .tasks
        .iter()
        .filter(|task| task.task_type == SurfaceTaskType::MainSession)
        .filter_map(|task| Some((&task.task_id, task.started_at?.get())))
        .collect();
    let mut last_item = HashMap::new();
    let mut tool_results = HashMap::new();
    for (index, item) in snapshot.items.iter().enumerate() {
        let turn = match item {
            SurfaceItem::ToolResultMessage {
                turn_id,
                tool_call_id,
                ..
            } => {
                tool_results.insert((turn_id, tool_call_id.as_str()), index + 1);
                turn_id
            }
            SurfaceItem::UserMessage { turn_id, .. }
            | SurfaceItem::AssistantMessage { turn_id, .. }
            | SurfaceItem::AssistantReasoning { turn_id, .. }
            | SurfaceItem::AssistantPlan { turn_id, .. } => turn_id,
            SurfaceItem::SystemMessage { .. } => continue,
        };
        last_item.insert(turn, index + 1);
    }
    let ordered = snapshot
        .operation_history
        .iter()
        .chain(&snapshot.foreground_operation);
    let operations: HashMap<_, _> = ordered
        .clone()
        .enumerate()
        .map(|(index, operation)| (&operation.operation_id, (index, operation)))
        .collect();
    let mut dated_turns = Vec::new();
    let mut last_undated = None;
    for (index, operation) in ordered.enumerate() {
        for (ordinal, turn) in operation.agent_loop_turns.iter().enumerate() {
            let position = (index, ordinal);
            match turn
                .admitted_main_task_id
                .as_ref()
                .and_then(|task| main_starts.get(task))
            {
                Some(start) => {
                    if let Some(boundary) = last_item.get(&turn.turn_id) {
                        dated_turns.push((*start, position, *boundary));
                    }
                }
                None => last_undated = Some(position),
            }
        }
    }
    dated_turns.sort_unstable();
    let summaries = crate::surface_projection::workflow_task_summaries(snapshot);
    let mut rows = Vec::new();
    let mut batches = HashSet::new();
    for (task, summary) in snapshot
        .tasks
        .iter()
        .zip(&summaries)
        .filter(|(task, _)| task.task_type == SurfaceTaskType::Subagent)
    {
        let agent = snapshot
            .subagents
            .iter()
            .find(|agent| agent.task_id == task.task_id);
        let operation = owner(task, agent).and_then(|id| operations.get(id));
        let turns = operation.into_iter().flat_map(|(_, op)| {
            op.initial_logical_turn_id
                .iter()
                .chain(
                    op.generations
                        .iter()
                        .map(|gen_record| &gen_record.logical_turn_id),
                )
                .chain(op.agent_loop_turns.iter().map(|turn| &turn.turn_id))
        });
        let owning_end = turns
            .clone()
            .filter_map(|turn| last_item.get(turn))
            .max()
            .copied()
            .unwrap_or(snapshot.items.len());
        let launch_item = agent.and_then(|agent| {
            turns
                .filter_map(|turn| tool_results.get(&(turn, agent.batch_id.as_str())))
                .min()
                .copied()
        });
        let key = batch_key(summary);
        if batches.insert(key) {
            rows.push((
                launch_item.unwrap_or(owning_end),
                task.created_at.get(),
                false,
                key,
                delegating_line(summary.subagent_batch_size.unwrap_or(1) as usize),
            ));
        }
        if let Some(text) = ended(summary) {
            let time = task.completed_at.map(|time| time.get());
            let boundary = time
                .and_then(|end| {
                    dated_turns[..dated_turns.partition_point(|(start, ..)| *start <= end)].last()
                })
                .filter(|(_, position, _)| {
                    !last_undated.is_some_and(|undated| {
                        undated > *position
                            && operation.is_some_and(|(owner_index, _)| undated.0 >= *owner_index)
                    })
                })
                .map_or(owning_end, |(_, _, boundary)| *boundary);
            rows.push((
                if launch_item.is_some() {
                    boundary
                } else {
                    boundary.max(owning_end)
                },
                time.unwrap_or(i64::MAX).max(task.created_at.get()),
                true,
                summary.id.as_str(),
                text,
            ));
        }
    }
    rows.sort_unstable();
    rows.into_iter()
        .map(|(boundary, _, _, _, text)| (boundary, text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording_for_every_agent_status_and_batch_size() {
        assert_eq!(delegating_line(1), "Delegating to 1 agent");
        assert_eq!(delegating_line(2), "Delegating to 2 agents in parallel");
        use TaskStatus::*;
        for (status, expected) in [
            (Completed, Some("Agent completed: scout")),
            (Cancelled, Some("Agent cancelled: scout")),
            (Stopped, Some("Agent stopped: scout")),
            (Blocked, Some("Agent blocked: offline")),
            (Failed, Some("Agent failed: offline")),
            (Queued, None),
            (Running, None),
            (ApprovalRequired, None),
            (Paused, None),
            (Stopping, None),
        ] {
            assert_eq!(
                agent_ended_line(status, "scout", Some("offline")).as_deref(),
                expected
            );
        }
        assert_eq!(
            agent_ended_line(Failed, "scout", None).as_deref(),
            Some("Agent failed: scout")
        );
        assert_eq!(
            agent_ended_line(Blocked, "scout", None).as_deref(),
            Some("Agent blocked: scout")
        );
    }
}
