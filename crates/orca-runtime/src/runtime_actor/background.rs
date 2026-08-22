use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::runtime_surface as surface;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum BackgroundAdmissionError {
    CapacityExceeded { capacity: usize },
    DuplicateTask { task_id: String },
}

impl BackgroundAdmissionError {
    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::CapacityExceeded { capacity } => *capacity,
            Self::DuplicateTask { .. } => usize::MAX,
        }
    }
}

pub(crate) trait ManagedBackgroundTask<Workflow, Provider> {
    fn cancel(&self);
    fn workflow(&self) -> Option<&Workflow>;
    fn provider(&self) -> Option<&Provider>;
    fn attach_workflow(&mut self, workflow: Workflow);
}

pub(crate) trait ScheduledBackgroundRetry {
    fn retry_at(&self) -> Instant;
    fn defer_until(&mut self, retry_at: Instant);
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum BackgroundRetryKey {
    WorkflowCompletion(surface::SurfaceOperationId),
    ProviderPreparation(surface::SurfaceOperationId),
    ProviderCompletion(surface::SurfaceOperationId),
    ApprovalResolution(surface::SurfaceOperationId),
    Control(surface::SurfaceOperationId),
}

pub(crate) enum BackgroundRetryEffect<
    WorkflowCompletion,
    ProviderPreparation,
    ProviderCompletion,
    ApprovalResolution,
    Control,
> {
    WorkflowCompletion {
        operation_id: surface::SurfaceOperationId,
        pending: WorkflowCompletion,
    },
    ProviderPreparation {
        operation_id: surface::SurfaceOperationId,
        pending: ProviderPreparation,
    },
    ProviderCompletion {
        operation_id: surface::SurfaceOperationId,
        pending: ProviderCompletion,
    },
    ApprovalResolution {
        operation_id: surface::SurfaceOperationId,
        pending: ApprovalResolution,
    },
    Control {
        operation_id: surface::SurfaceOperationId,
        pending: Control,
    },
}

pub(crate) enum BackgroundRetryResolution {
    Settled,
    RetryAt(Instant),
}

pub(crate) struct BackgroundOperationController<
    Task,
    Workflow,
    Provider,
    WorkflowCompletion,
    ProviderPreparation,
    ProviderCompletion,
    ApprovalResolution,
    Control,
    TaskOwnership = (),
> {
    tasks: HashMap<String, Task>,
    capacity: usize,
    completion_tx: mpsc::UnboundedSender<String>,
    completion_rx: mpsc::UnboundedReceiver<String>,
    workflow_completions: HashMap<surface::SurfaceOperationId, WorkflowCompletion>,
    provider_preparations: HashMap<surface::SurfaceOperationId, ProviderPreparation>,
    provider_completions: HashMap<surface::SurfaceOperationId, ProviderCompletion>,
    approval_resolutions: HashMap<surface::SurfaceOperationId, ApprovalResolution>,
    controls: HashMap<surface::SurfaceOperationId, Control>,
    task_ownership: Option<TaskOwnership>,
    _workflow: std::marker::PhantomData<Workflow>,
    _provider: std::marker::PhantomData<Provider>,
}

pub(crate) type TaskWorkflowController<
    Task,
    Workflow,
    Provider,
    WorkflowCompletion,
    ProviderPreparation,
    ProviderCompletion,
    ApprovalResolution,
    Control,
    TaskOwnership,
> = BackgroundOperationController<
    Task,
    Workflow,
    Provider,
    WorkflowCompletion,
    ProviderPreparation,
    ProviderCompletion,
    ApprovalResolution,
    Control,
    TaskOwnership,
>;

impl<
    Task,
    Workflow,
    Provider,
    WorkflowCompletion,
    ProviderPreparation,
    ProviderCompletion,
    ApprovalResolution,
    Control,
    TaskOwnership,
>
    BackgroundOperationController<
        Task,
        Workflow,
        Provider,
        WorkflowCompletion,
        ProviderPreparation,
        ProviderCompletion,
        ApprovalResolution,
        Control,
        TaskOwnership,
    >
where
    Task: ManagedBackgroundTask<Workflow, Provider>,
    Workflow: Clone,
    Provider: Clone,
    WorkflowCompletion: ScheduledBackgroundRetry,
    ProviderPreparation: ScheduledBackgroundRetry,
    ProviderCompletion: ScheduledBackgroundRetry,
    ApprovalResolution: ScheduledBackgroundRetry,
    Control: ScheduledBackgroundRetry,
{
    pub(crate) fn new(capacity: usize) -> Self {
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        Self {
            tasks: HashMap::new(),
            capacity,
            completion_tx,
            completion_rx,
            workflow_completions: HashMap::new(),
            provider_preparations: HashMap::new(),
            provider_completions: HashMap::new(),
            approval_resolutions: HashMap::new(),
            controls: HashMap::new(),
            task_ownership: None,
            _workflow: std::marker::PhantomData,
            _provider: std::marker::PhantomData,
        }
    }

    pub(crate) fn task_ownership(&self) -> Option<&TaskOwnership> {
        self.task_ownership.as_ref()
    }

    pub(crate) fn take_task_ownership(&mut self) -> Option<TaskOwnership> {
        self.task_ownership.take()
    }

    pub(crate) fn retain_task_ownership(&mut self, ownership: TaskOwnership) {
        self.task_ownership = Some(ownership);
    }

    pub(crate) fn ensure_capacity(
        &self,
        additional: usize,
    ) -> Result<(), BackgroundAdmissionError> {
        if self.tasks.len().saturating_add(additional) > self.capacity {
            return Err(BackgroundAdmissionError::CapacityExceeded {
                capacity: self.capacity,
            });
        }
        Ok(())
    }

    pub(crate) fn has_capacity(&self, additional: usize) -> bool {
        self.ensure_capacity(additional).is_ok()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(crate) fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values()
    }

    pub(crate) fn admit_task(
        &mut self,
        task_id: String,
        task: Task,
    ) -> Result<(), BackgroundAdmissionError> {
        if self.tasks.contains_key(&task_id) {
            task.cancel();
            return Err(BackgroundAdmissionError::DuplicateTask { task_id });
        }
        self.ensure_capacity(1)?;
        self.tasks.insert(task_id, task);
        Ok(())
    }

    pub(crate) fn attach_workflow(&mut self, task_id: &str, workflow: Workflow) -> bool {
        let Some(task) = self.tasks.get_mut(task_id) else {
            return false;
        };
        task.attach_workflow(workflow);
        true
    }

    pub(crate) fn has_provider_matching(&self, matches: impl Fn(&Provider) -> bool) -> bool {
        self.tasks
            .values()
            .filter_map(ManagedBackgroundTask::provider)
            .any(matches)
    }

    pub(crate) fn find_provider_matching(
        &self,
        matches: impl Fn(&Provider) -> bool,
    ) -> Option<Provider> {
        self.tasks
            .values()
            .filter_map(ManagedBackgroundTask::provider)
            .find(|provider| matches(provider))
            .cloned()
    }

    pub(crate) fn find_workflow_matching(
        &self,
        matches: impl Fn(&Workflow) -> bool,
    ) -> Option<Workflow> {
        self.tasks
            .values()
            .filter_map(ManagedBackgroundTask::workflow)
            .find(|workflow| matches(workflow))
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn provider_for_task(&self, task_id: &str) -> Option<Provider> {
        self.tasks
            .get(task_id)
            .and_then(ManagedBackgroundTask::provider)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn workflow_for_task(&self, task_id: &str) -> Option<Workflow> {
        self.tasks
            .get(task_id)
            .and_then(ManagedBackgroundTask::workflow)
            .cloned()
    }

    pub(crate) fn cancel_task(&self, task_id: &str) -> bool {
        let Some(task) = self.tasks.get(task_id) else {
            return false;
        };
        task.cancel();
        true
    }

    pub(crate) fn begin_completion(&mut self, task_id: &str) -> Option<Task> {
        self.tasks.remove(task_id)
    }

    pub(crate) fn begin_shutdown(&mut self) -> Vec<Task> {
        for task in self.tasks.values() {
            task.cancel();
        }
        self.tasks.drain().map(|(_, task)| task).collect()
    }

    pub(crate) fn completion_notifier(&self) -> mpsc::UnboundedSender<String> {
        self.completion_tx.clone()
    }

    pub(crate) async fn recv_completion(&mut self) -> Option<String> {
        self.completion_rx.recv().await
    }

    pub(crate) fn has_pending_completion(&self) -> bool {
        !self.workflow_completions.is_empty()
            || !self.provider_preparations.is_empty()
            || !self.provider_completions.is_empty()
            || !self.approval_resolutions.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn has_pending_completion_operation(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> bool {
        self.workflow_completions.contains_key(operation_id)
            || self.provider_preparations.contains_key(operation_id)
            || self.provider_completions.contains_key(operation_id)
            || self.approval_resolutions.contains_key(operation_id)
    }

    pub(crate) fn pending_completion_operation_ids(&self) -> Vec<surface::SurfaceOperationId> {
        let mut ids = self
            .workflow_completions
            .keys()
            .chain(self.provider_preparations.keys())
            .chain(self.provider_completions.keys())
            .chain(self.approval_resolutions.keys())
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        ids
    }

    pub(crate) fn has_pending_control(&self) -> bool {
        !self.controls.is_empty()
    }

    pub(crate) fn pending_control_operation_ids(&self) -> Vec<surface::SurfaceOperationId> {
        let mut ids = self.controls.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[cfg(test)]
    pub(crate) fn pending_operation_ids(&self) -> Vec<surface::SurfaceOperationId> {
        let mut ids = self
            .workflow_completions
            .keys()
            .chain(self.provider_preparations.keys())
            .chain(self.provider_completions.keys())
            .chain(self.approval_resolutions.keys())
            .chain(self.controls.keys())
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        ids
    }

    pub(crate) fn retain_workflow_completion(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        pending: WorkflowCompletion,
    ) {
        self.workflow_completions.insert(operation_id, pending);
    }

    pub(crate) fn retain_provider_preparation(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        pending: ProviderPreparation,
    ) {
        self.provider_preparations.insert(operation_id, pending);
    }

    pub(crate) fn retain_provider_completion(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        pending: ProviderCompletion,
    ) {
        self.provider_completions.insert(operation_id, pending);
    }

    pub(crate) fn retain_approval_resolution(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        pending: ApprovalResolution,
    ) {
        self.approval_resolutions.insert(operation_id, pending);
    }

    pub(crate) fn retain_control(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        pending: Control,
    ) {
        self.controls.insert(operation_id, pending);
    }

    pub(crate) fn next_retry(&self) -> Option<(Instant, BackgroundRetryKey)> {
        self.workflow_completions
            .iter()
            .map(|(id, pending)| {
                (
                    pending.retry_at(),
                    BackgroundRetryKey::WorkflowCompletion(id.clone()),
                )
            })
            .chain(self.provider_preparations.iter().map(|(id, pending)| {
                (
                    pending.retry_at(),
                    BackgroundRetryKey::ProviderPreparation(id.clone()),
                )
            }))
            .chain(self.provider_completions.iter().map(|(id, pending)| {
                (
                    pending.retry_at(),
                    BackgroundRetryKey::ProviderCompletion(id.clone()),
                )
            }))
            .chain(self.approval_resolutions.iter().map(|(id, pending)| {
                (
                    pending.retry_at(),
                    BackgroundRetryKey::ApprovalResolution(id.clone()),
                )
            }))
            .chain(
                self.controls.iter().map(|(id, pending)| {
                    (pending.retry_at(), BackgroundRetryKey::Control(id.clone()))
                }),
            )
            .min()
    }

    pub(crate) fn next_retry_at(&self) -> Option<Instant> {
        self.next_retry().map(|(retry_at, _)| retry_at)
    }

    pub(crate) fn begin_retry(
        &mut self,
        key: &BackgroundRetryKey,
    ) -> Option<
        BackgroundRetryEffect<
            WorkflowCompletion,
            ProviderPreparation,
            ProviderCompletion,
            ApprovalResolution,
            Control,
        >,
    > {
        match key {
            BackgroundRetryKey::WorkflowCompletion(operation_id) => self
                .workflow_completions
                .remove(operation_id)
                .map(|pending| BackgroundRetryEffect::WorkflowCompletion {
                    operation_id: operation_id.clone(),
                    pending,
                }),
            BackgroundRetryKey::ProviderPreparation(operation_id) => self
                .provider_preparations
                .remove(operation_id)
                .map(|pending| BackgroundRetryEffect::ProviderPreparation {
                    operation_id: operation_id.clone(),
                    pending,
                }),
            BackgroundRetryKey::ProviderCompletion(operation_id) => self
                .provider_completions
                .remove(operation_id)
                .map(|pending| BackgroundRetryEffect::ProviderCompletion {
                    operation_id: operation_id.clone(),
                    pending,
                }),
            BackgroundRetryKey::ApprovalResolution(operation_id) => self
                .approval_resolutions
                .remove(operation_id)
                .map(|pending| BackgroundRetryEffect::ApprovalResolution {
                    operation_id: operation_id.clone(),
                    pending,
                }),
            BackgroundRetryKey::Control(operation_id) => {
                self.controls
                    .remove(operation_id)
                    .map(|pending| BackgroundRetryEffect::Control {
                        operation_id: operation_id.clone(),
                        pending,
                    })
            }
        }
    }

    pub(crate) fn resolve_retry(
        &mut self,
        mut effect: BackgroundRetryEffect<
            WorkflowCompletion,
            ProviderPreparation,
            ProviderCompletion,
            ApprovalResolution,
            Control,
        >,
        resolution: BackgroundRetryResolution,
    ) {
        match resolution {
            BackgroundRetryResolution::Settled => {}
            BackgroundRetryResolution::RetryAt(retry_at) => {
                Self::defer_effect(&mut effect, retry_at);
                self.retain_effect(effect);
            }
        }
    }

    fn defer_effect(
        effect: &mut BackgroundRetryEffect<
            WorkflowCompletion,
            ProviderPreparation,
            ProviderCompletion,
            ApprovalResolution,
            Control,
        >,
        retry_at: Instant,
    ) {
        match effect {
            BackgroundRetryEffect::WorkflowCompletion { pending, .. } => {
                pending.defer_until(retry_at)
            }
            BackgroundRetryEffect::ProviderPreparation { pending, .. } => {
                pending.defer_until(retry_at)
            }
            BackgroundRetryEffect::ProviderCompletion { pending, .. } => {
                pending.defer_until(retry_at)
            }
            BackgroundRetryEffect::ApprovalResolution { pending, .. } => {
                pending.defer_until(retry_at)
            }
            BackgroundRetryEffect::Control { pending, .. } => pending.defer_until(retry_at),
        }
    }

    fn retain_effect(
        &mut self,
        effect: BackgroundRetryEffect<
            WorkflowCompletion,
            ProviderPreparation,
            ProviderCompletion,
            ApprovalResolution,
            Control,
        >,
    ) {
        match effect {
            BackgroundRetryEffect::WorkflowCompletion {
                operation_id,
                pending,
            } => self.retain_workflow_completion(operation_id, pending),
            BackgroundRetryEffect::ProviderPreparation {
                operation_id,
                pending,
            } => self.retain_provider_preparation(operation_id, pending),
            BackgroundRetryEffect::ProviderCompletion {
                operation_id,
                pending,
            } => self.retain_provider_completion(operation_id, pending),
            BackgroundRetryEffect::ApprovalResolution {
                operation_id,
                pending,
            } => self.retain_approval_resolution(operation_id, pending),
            BackgroundRetryEffect::Control {
                operation_id,
                pending,
            } => self.retain_control(operation_id, pending),
        }
    }

    #[cfg(test)]
    fn trace(&self) -> BackgroundControllerTrace {
        BackgroundControllerTrace {
            tasks: self.tasks.len(),
            capacity: self.capacity,
            pending: self.pending_operation_ids().len(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct BackgroundControllerTrace {
    tasks: usize,
    capacity: usize,
    pending: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[derive(Clone)]
    struct Retry(Instant);

    impl ScheduledBackgroundRetry for Retry {
        fn retry_at(&self) -> Instant {
            self.0
        }
        fn defer_until(&mut self, retry_at: Instant) {
            self.0 = retry_at;
        }
    }

    struct Task {
        cancelled: Arc<AtomicBool>,
        workflow: Option<u8>,
        provider: Option<u8>,
    }

    impl ManagedBackgroundTask<u8, u8> for Task {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::Release);
        }
        fn workflow(&self) -> Option<&u8> {
            self.workflow.as_ref()
        }
        fn provider(&self) -> Option<&u8> {
            self.provider.as_ref()
        }
        fn attach_workflow(&mut self, workflow: u8) {
            self.workflow = Some(workflow);
        }
    }

    fn operation_id() -> surface::SurfaceOperationId {
        surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap()
    }

    #[test]
    fn background_controller_trace_equivalence() {
        let mut controller =
            BackgroundOperationController::<Task, u8, u8, Retry, Retry, Retry, Retry, Retry>::new(
                1,
            );
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = Task {
            cancelled: cancelled.clone(),
            workflow: None,
            provider: Some(7),
        };
        let mut trace = vec![controller.trace()];
        controller.admit_task("first".into(), task).unwrap();
        assert!(controller.attach_workflow("first", 9));
        assert_eq!(controller.workflow_for_task("first"), Some(9));
        assert_eq!(controller.provider_for_task("first"), Some(7));
        let duplicate_cancelled = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            controller.admit_task(
                "first".into(),
                Task {
                    cancelled: duplicate_cancelled.clone(),
                    workflow: None,
                    provider: None
                }
            ),
            Err(BackgroundAdmissionError::DuplicateTask { task_id }) if task_id == "first"
        ));
        assert!(duplicate_cancelled.load(Ordering::Acquire));
        assert!(matches!(
            controller.admit_task(
                "second".into(),
                Task {
                    cancelled: cancelled.clone(),
                    workflow: None,
                    provider: None
                }
            ),
            Err(BackgroundAdmissionError::CapacityExceeded { capacity: 1 })
        ));
        trace.push(controller.trace());

        let operation_id = operation_id();
        let first_retry = Instant::now();
        controller.retain_approval_resolution(operation_id.clone(), Retry(first_retry));
        assert!(controller.has_pending_completion());
        assert!(controller.has_pending_completion_operation(&operation_id));
        assert_eq!(
            controller.pending_completion_operation_ids(),
            vec![operation_id.clone()]
        );
        let approval_key = BackgroundRetryKey::ApprovalResolution(operation_id.clone());
        let approval_effect = controller.begin_retry(&approval_key).unwrap();
        controller.resolve_retry(approval_effect, BackgroundRetryResolution::Settled);
        controller.retain_provider_completion(operation_id.clone(), Retry(first_retry));
        trace.push(controller.trace());
        let (_, key) = controller.next_retry().unwrap();
        let effect = controller.begin_retry(&key).unwrap();
        let deferred = first_retry + std::time::Duration::from_secs(1);
        controller.resolve_retry(effect, BackgroundRetryResolution::RetryAt(deferred));
        assert_eq!(controller.next_retry().unwrap().0, deferred);
        let effect = controller.begin_retry(&key).unwrap();
        controller.resolve_retry(effect, BackgroundRetryResolution::Settled);
        trace.push(controller.trace());

        controller
            .completion_notifier()
            .send("first".into())
            .unwrap();
        assert_eq!(controller.completion_rx.try_recv().unwrap(), "first");
        let task = controller.begin_completion("first").unwrap();
        task.cancel();
        assert!(cancelled.load(Ordering::Acquire));
        trace.push(controller.trace());
        assert_eq!(
            trace,
            vec![
                BackgroundControllerTrace {
                    tasks: 0,
                    capacity: 1,
                    pending: 0
                },
                BackgroundControllerTrace {
                    tasks: 1,
                    capacity: 1,
                    pending: 0
                },
                BackgroundControllerTrace {
                    tasks: 1,
                    capacity: 1,
                    pending: 1
                },
                BackgroundControllerTrace {
                    tasks: 1,
                    capacity: 1,
                    pending: 0
                },
                BackgroundControllerTrace {
                    tasks: 0,
                    capacity: 1,
                    pending: 0
                },
            ]
        );
    }
}
