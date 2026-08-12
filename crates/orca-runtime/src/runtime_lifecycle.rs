use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
use orca_core::thread_identity::TurnId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSessionLifecycle {
    run_id: String,
    active_task: Option<RuntimeTaskLifecycle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTaskLifecycle {
    id: String,
    kind: RuntimeTaskKind,
    status: RuntimeTaskStatus,
    current_turn: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskKind {
    Agent,
    Workflow,
    Subagent,
    Shell,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    ApprovalRequired,
    BudgetExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnLifecycle {
    number: u32,
}

pub struct RuntimeTurnRunner<'a> {
    lifecycle: &'a mut RuntimeSessionLifecycle,
}

#[derive(Debug)]
pub struct RuntimeStartedTurn {
    pub(crate) turn: u32,
    pub(crate) task: Option<RuntimeTaskLifecycle>,
    pub event: EventDraft,
}

#[derive(Clone, Debug)]
pub struct RuntimeAdvancedTurn {
    pub(crate) turn: u32,
    pub(crate) task: Option<RuntimeTaskLifecycle>,
}

impl RuntimeSessionLifecycle {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            active_task: None,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn start_task(&mut self, kind: RuntimeTaskKind) -> &RuntimeTaskLifecycle {
        let id = format!("{}:task-1", self.run_id);
        self.start_task_with_id(kind, id)
    }

    pub fn start_task_with_id(
        &mut self,
        kind: RuntimeTaskKind,
        id: impl Into<String>,
    ) -> &RuntimeTaskLifecycle {
        self.active_task = Some(RuntimeTaskLifecycle {
            id: id.into(),
            kind,
            status: RuntimeTaskStatus::Running,
            current_turn: 0,
        });
        self.active_task.as_ref().expect("task just started")
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.active_task.as_ref()
    }

    pub fn next_turn(&mut self) -> RuntimeTurnLifecycle {
        let task = self
            .active_task
            .get_or_insert_with(|| RuntimeTaskLifecycle {
                id: format!("{}:task-1", self.run_id),
                kind: RuntimeTaskKind::Agent,
                status: RuntimeTaskStatus::Running,
                current_turn: 0,
            });
        task.current_turn = task.current_turn.saturating_add(1);
        RuntimeTurnLifecycle {
            number: task.current_turn,
        }
    }

    pub fn finish_task(&mut self, status: RunStatus) -> Option<&RuntimeTaskLifecycle> {
        let task = self.active_task.as_mut()?;
        task.status = RuntimeTaskStatus::from_run_status(status);
        Some(task)
    }
}

impl<'a> RuntimeTurnRunner<'a> {
    pub fn new(lifecycle: &'a mut RuntimeSessionLifecycle) -> Self {
        Self { lifecycle }
    }

    pub fn start_turn(
        &mut self,
        events: &mut EventFactory,
        turn_id: &TurnId,
        prompt: Option<&str>,
    ) -> RuntimeStartedTurn {
        let advanced = self.advance_turn();
        let event = advanced
            .task
            .as_ref()
            .map(|task| {
                RuntimeTurnLifecycle {
                    number: advanced.turn,
                }
                .started_event(events, turn_id, prompt, task)
            })
            .unwrap_or_else(|| events.turn_started(turn_id, advanced.turn, prompt));
        RuntimeStartedTurn {
            turn: advanced.turn,
            task: advanced.task,
            event,
        }
    }

    pub fn advance_turn(&mut self) -> RuntimeAdvancedTurn {
        let turn = self.lifecycle.next_turn();
        let task = self.lifecycle.active_task().cloned();
        RuntimeAdvancedTurn {
            turn: turn.number(),
            task,
        }
    }
}

impl RuntimeStartedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }
}

impl RuntimeAdvancedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }
}

impl RuntimeTaskLifecycle {
    pub fn new_snapshot(
        id: impl Into<String>,
        kind: RuntimeTaskKind,
        status: RuntimeTaskStatus,
        current_turn: u32,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            status,
            current_turn,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> RuntimeTaskKind {
        self.kind
    }

    pub fn status(&self) -> RuntimeTaskStatus {
        self.status
    }

    pub fn current_turn(&self) -> u32 {
        self.current_turn
    }

    pub fn payload(&self) -> Value {
        json!({
            "task_id": self.id,
            "kind": self.kind,
            "status": self.status,
            "turn": self.current_turn
        })
    }

    pub fn with_status(&self, status: RuntimeTaskStatus) -> Self {
        let mut task = self.clone();
        task.status = status;
        task
    }

    pub fn attach_to_event(&self, mut event: EventDraft) -> EventDraft {
        event.payload["task"] = self.payload();
        event
    }
}

impl RuntimeTaskStatus {
    pub(crate) fn from_run_status(status: RunStatus) -> Self {
        match status {
            RunStatus::Success => Self::Succeeded,
            RunStatus::Failed | RunStatus::VerificationFailed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::ApprovalRequired => Self::ApprovalRequired,
        }
    }
}

impl RuntimeTurnLifecycle {
    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn started_event(
        self,
        events: &mut EventFactory,
        turn_id: &TurnId,
        prompt: Option<&str>,
        task: &RuntimeTaskLifecycle,
    ) -> EventDraft {
        let mut event = events.turn_started(turn_id, self.number, prompt);
        event.payload["task"] = task.payload();
        event
    }
}
