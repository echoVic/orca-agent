use std::collections::HashMap;
use std::sync::mpsc::SyncSender;

use tokio::time::Instant;

use crate::runtime_actor::RuntimeActorEffect;
use crate::runtime_surface as surface;

pub(crate) trait ScheduledSurfaceCommit {
    fn operation_id(&self) -> &surface::SurfaceOperationId;
    fn retry_at(&self) -> Instant;
    fn defer_until(&mut self, retry_at: Instant);
}

pub(crate) trait GoalRecoverySurfaceCommit {
    fn owns_goal_recovery(&self) -> bool;
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SurfaceCommitRetryKey {
    AdmissionCommit(surface::SurfaceOperationId),
    AdmissionRepair(surface::SurfaceOperationId),
    AdmissionTerminal(surface::SurfaceOperationId),
    Terminalization(surface::SurfaceOperationId),
}

#[derive(Clone)]
pub(crate) enum SurfaceCommitEffect<
    Terminalization,
    AdmissionCommit,
    AdmissionRepair,
    AdmissionTerminal,
> {
    Terminalization(Terminalization),
    AdmissionCommit(AdmissionCommit),
    AdmissionRepair(AdmissionRepair),
    AdmissionTerminal(AdmissionTerminal),
}

#[cfg(test)]
impl<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal>
    SurfaceCommitEffect<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal>
where
    Terminalization: ScheduledSurfaceCommit,
    AdmissionCommit: ScheduledSurfaceCommit,
    AdmissionRepair: ScheduledSurfaceCommit,
    AdmissionTerminal: ScheduledSurfaceCommit,
{
    pub(crate) fn key(&self) -> SurfaceCommitRetryKey {
        match self {
            Self::Terminalization(pending) => {
                SurfaceCommitRetryKey::Terminalization(pending.operation_id().clone())
            }
            Self::AdmissionCommit(pending) => {
                SurfaceCommitRetryKey::AdmissionCommit(pending.operation_id().clone())
            }
            Self::AdmissionRepair(pending) => {
                SurfaceCommitRetryKey::AdmissionRepair(pending.operation_id().clone())
            }
            Self::AdmissionTerminal(pending) => {
                SurfaceCommitRetryKey::AdmissionTerminal(pending.operation_id().clone())
            }
        }
    }
}

pub(crate) enum SurfaceCommitResolution {
    Committed,
    RetryAt(Instant),
    #[cfg(test)]
    Aborted,
}

pub(crate) struct SurfaceCommitController<
    Terminalization,
    AdmissionCommit,
    AdmissionRepair,
    AdmissionTerminal,
    PendingTerminal,
> {
    terminals: HashMap<surface::SurfaceOperationId, surface::OperationTerminalAtCursor>,
    pending_terminal_commits: HashMap<surface::SurfaceOperationId, PendingTerminal>,
    terminal_waiters: HashMap<surface::SurfaceOperationId, Vec<SurfaceTerminalWaiter>>,
    terminalization: Option<Terminalization>,
    admission_commits: HashMap<surface::SurfaceOperationId, AdmissionCommit>,
    admission_repairs: HashMap<surface::SurfaceOperationId, AdmissionRepair>,
    admission_terminals: HashMap<surface::SurfaceOperationId, AdmissionTerminal>,
}

struct SurfaceTerminalWaiter {
    reply: SyncSender<
        Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
    >,
    caller_cancel: surface::OptionalProcessLocalCancel,
}

impl<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal, PendingTerminal>
    SurfaceCommitController<
        Terminalization,
        AdmissionCommit,
        AdmissionRepair,
        AdmissionTerminal,
        PendingTerminal,
    >
where
    Terminalization: ScheduledSurfaceCommit + Clone,
    AdmissionCommit: ScheduledSurfaceCommit + Clone,
    AdmissionRepair: ScheduledSurfaceCommit + GoalRecoverySurfaceCommit + Clone,
    AdmissionTerminal: ScheduledSurfaceCommit + GoalRecoverySurfaceCommit + Clone,
{
    pub(crate) fn new(
        terminals: HashMap<surface::SurfaceOperationId, surface::OperationTerminalAtCursor>,
    ) -> Self {
        Self {
            terminals,
            pending_terminal_commits: HashMap::new(),
            terminal_waiters: HashMap::new(),
            terminalization: None,
            admission_commits: HashMap::new(),
            admission_repairs: HashMap::new(),
            admission_terminals: HashMap::new(),
        }
    }

    pub(crate) fn terminal(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&surface::OperationTerminalAtCursor> {
        self.terminals.get(operation_id)
    }

    pub(crate) fn terminal_values(
        &self,
    ) -> impl Iterator<Item = &surface::OperationTerminalAtCursor> {
        self.terminals.values()
    }

    pub(crate) fn cache_terminal(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        terminal: surface::OperationTerminalAtCursor,
    ) -> Vec<RuntimeActorEffect> {
        let result = surface::WaitOperationTerminalResult::Terminal {
            value: terminal.clone(),
        };
        let waiter_operation_id = operation_id.clone();
        self.terminals.insert(operation_id, terminal);
        self.terminal_waiters
            .remove(&waiter_operation_id)
            .unwrap_or_default()
            .into_iter()
            .map(|waiter| RuntimeActorEffect::ReplyOperation {
                reply: waiter.reply,
                result: Ok(result.clone()),
                nonblocking: false,
            })
            .collect()
    }

    pub(crate) fn pending_terminal(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&PendingTerminal> {
        self.pending_terminal_commits.get(operation_id)
    }

    pub(crate) fn has_pending_terminal(&self, operation_id: &surface::SurfaceOperationId) -> bool {
        self.pending_terminal_commits.contains_key(operation_id)
    }

    pub(crate) fn pending_terminals_empty(&self) -> bool {
        self.pending_terminal_commits.is_empty()
    }

    pub(crate) fn retain_pending_terminal(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        terminal: PendingTerminal,
    ) {
        self.pending_terminal_commits.insert(operation_id, terminal);
    }

    pub(crate) fn register_terminal_waiter(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        waiter: SyncSender<
            Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        >,
        caller_cancel: surface::OptionalProcessLocalCancel,
    ) {
        self.terminal_waiters
            .entry(operation_id)
            .or_default()
            .push(SurfaceTerminalWaiter {
                reply: waiter,
                caller_cancel,
            });
    }

    pub(crate) fn has_terminal_waiters(&self) -> bool {
        !self.terminal_waiters.is_empty()
    }

    pub(crate) fn cancelled_terminal_waiters(&mut self) -> Vec<RuntimeActorEffect> {
        let mut effects = Vec::new();
        let mut remaining = HashMap::new();
        for (operation_id, waiters) in std::mem::take(&mut self.terminal_waiters) {
            let mut live = Vec::with_capacity(waiters.len());
            for waiter in waiters {
                if waiter.caller_cancel.is_cancelled() {
                    effects.push(RuntimeActorEffect::ReplyOperation {
                        reply: waiter.reply,
                        result: Ok(surface::WaitOperationTerminalResult::WaitCancelled {
                            operation_id: operation_id.clone(),
                        }),
                        nonblocking: true,
                    });
                } else {
                    live.push(waiter);
                }
            }
            if !live.is_empty() {
                remaining.insert(operation_id, live);
            }
        }
        self.terminal_waiters = remaining;
        effects
    }

    #[cfg(test)]
    pub(crate) fn terminal_waiter_count(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> usize {
        self.terminal_waiters.get(operation_id).map_or(0, Vec::len)
    }

    pub(crate) fn take_terminal_waiters(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) -> Vec<
        SyncSender<
            Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        >,
    > {
        self.terminal_waiters
            .remove(operation_id)
            .unwrap_or_default()
            .into_iter()
            .map(|waiter| waiter.reply)
            .collect()
    }

    pub(crate) fn settle_terminal_waiters(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
        result: Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        nonblocking: bool,
    ) -> Vec<RuntimeActorEffect> {
        self.take_terminal_waiters(operation_id)
            .into_iter()
            .map(|reply| RuntimeActorEffect::ReplyOperation {
                reply,
                result: result.clone(),
                nonblocking,
            })
            .collect()
    }

    pub(crate) fn has_terminalization(&self) -> bool {
        self.terminalization.is_some()
    }

    pub(crate) fn has_pending_admission(&self) -> bool {
        !self.admission_commits.is_empty()
            || !self.admission_repairs.is_empty()
            || !self.admission_terminals.is_empty()
    }

    pub(crate) fn has_admission_repair(&self, operation_id: &surface::SurfaceOperationId) -> bool {
        self.admission_repairs.contains_key(operation_id)
    }

    #[cfg(test)]
    pub(crate) fn admission_repair(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&AdmissionRepair> {
        self.admission_repairs.get(operation_id)
    }

    pub(crate) fn has_admission_terminal(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> bool {
        self.admission_terminals.contains_key(operation_id)
    }

    pub(crate) fn admission_terminal(
        &self,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&AdmissionTerminal> {
        self.admission_terminals.get(operation_id)
    }

    pub(crate) fn has_goal_recovery_owner(&self) -> bool {
        self.admission_repairs
            .values()
            .any(GoalRecoverySurfaceCommit::owns_goal_recovery)
            || self
                .admission_terminals
                .values()
                .any(GoalRecoverySurfaceCommit::owns_goal_recovery)
    }

    pub(crate) fn goal_recovery_operation_id(&self) -> Option<surface::SurfaceOperationId> {
        self.admission_repairs
            .iter()
            .filter(|(_, pending)| pending.owns_goal_recovery())
            .map(|(operation_id, _)| operation_id)
            .chain(
                self.admission_terminals
                    .iter()
                    .filter(|(_, pending)| pending.owns_goal_recovery())
                    .map(|(operation_id, _)| operation_id),
            )
            .min()
            .cloned()
    }

    pub(crate) fn prepare_terminalization(
        &mut self,
        pending: Terminalization,
    ) -> Result<
        SurfaceCommitEffect<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal>,
        Terminalization,
    > {
        if self.terminalization.is_some() {
            return Err(pending);
        }
        let effect = SurfaceCommitEffect::Terminalization(pending.clone());
        self.terminalization = Some(pending);
        Ok(effect)
    }

    pub(crate) fn prepare_admission_commit(&mut self, pending: AdmissionCommit) {
        self.admission_commits
            .insert(pending.operation_id().clone(), pending);
    }

    pub(crate) fn prepare_admission_repair(&mut self, pending: AdmissionRepair) {
        self.admission_repairs
            .insert(pending.operation_id().clone(), pending);
    }

    pub(crate) fn prepare_admission_terminal(&mut self, pending: AdmissionTerminal) {
        self.admission_terminals
            .insert(pending.operation_id().clone(), pending);
    }

    pub(crate) fn next_retry(&self) -> Option<(Instant, SurfaceCommitRetryKey)> {
        self.admission_commits
            .iter()
            .map(|(operation_id, pending)| {
                (
                    pending.retry_at(),
                    SurfaceCommitRetryKey::AdmissionCommit(operation_id.clone()),
                )
            })
            .chain(
                self.admission_repairs
                    .iter()
                    .map(|(operation_id, pending)| {
                        (
                            pending.retry_at(),
                            SurfaceCommitRetryKey::AdmissionRepair(operation_id.clone()),
                        )
                    }),
            )
            .chain(
                self.admission_terminals
                    .iter()
                    .map(|(operation_id, pending)| {
                        (
                            pending.retry_at(),
                            SurfaceCommitRetryKey::AdmissionTerminal(operation_id.clone()),
                        )
                    }),
            )
            .chain(self.terminalization.iter().map(|pending| {
                (
                    pending.retry_at(),
                    SurfaceCommitRetryKey::Terminalization(pending.operation_id().clone()),
                )
            }))
            .min()
    }

    pub(crate) fn next_retry_at(&self) -> Option<Instant> {
        self.next_retry().map(|(retry_at, _)| retry_at)
    }

    pub(crate) fn begin_attempt(
        &mut self,
        key: &SurfaceCommitRetryKey,
    ) -> Option<
        SurfaceCommitEffect<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal>,
    > {
        match key {
            SurfaceCommitRetryKey::Terminalization(operation_id) => {
                if self
                    .terminalization
                    .as_ref()
                    .is_none_or(|pending| pending.operation_id() != operation_id)
                {
                    return None;
                }
                let pending = self.terminalization.take()?;
                Some(SurfaceCommitEffect::Terminalization(pending))
            }
            SurfaceCommitRetryKey::AdmissionCommit(operation_id) => self
                .admission_commits
                .remove(operation_id)
                .map(SurfaceCommitEffect::AdmissionCommit),
            SurfaceCommitRetryKey::AdmissionRepair(operation_id) => self
                .admission_repairs
                .remove(operation_id)
                .map(SurfaceCommitEffect::AdmissionRepair),
            SurfaceCommitRetryKey::AdmissionTerminal(operation_id) => self
                .admission_terminals
                .remove(operation_id)
                .map(SurfaceCommitEffect::AdmissionTerminal),
        }
    }

    pub(crate) fn resolve_attempt(
        &mut self,
        mut effect: SurfaceCommitEffect<
            Terminalization,
            AdmissionCommit,
            AdmissionRepair,
            AdmissionTerminal,
        >,
        resolution: SurfaceCommitResolution,
    ) -> Option<
        SurfaceCommitEffect<Terminalization, AdmissionCommit, AdmissionRepair, AdmissionTerminal>,
    > {
        match resolution {
            SurfaceCommitResolution::Committed => Some(effect),
            #[cfg(test)]
            SurfaceCommitResolution::Aborted => Some(effect),
            SurfaceCommitResolution::RetryAt(retry_at) => {
                match &mut effect {
                    SurfaceCommitEffect::Terminalization(pending) => pending.defer_until(retry_at),
                    SurfaceCommitEffect::AdmissionCommit(pending) => pending.defer_until(retry_at),
                    SurfaceCommitEffect::AdmissionRepair(pending) => pending.defer_until(retry_at),
                    SurfaceCommitEffect::AdmissionTerminal(pending) => {
                        pending.defer_until(retry_at)
                    }
                }
                self.retain_effect(effect);
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn inspect_terminalization<R>(
        &self,
        inspect: impl FnOnce(&Terminalization) -> R,
    ) -> Option<R> {
        self.terminalization.as_ref().map(inspect)
    }

    fn retain_effect(
        &mut self,
        effect: SurfaceCommitEffect<
            Terminalization,
            AdmissionCommit,
            AdmissionRepair,
            AdmissionTerminal,
        >,
    ) {
        match effect {
            SurfaceCommitEffect::Terminalization(pending) => {
                assert!(
                    self.terminalization.is_none(),
                    "terminalization retry slot must be empty after begin_attempt"
                );
                self.terminalization = Some(pending);
            }
            SurfaceCommitEffect::AdmissionCommit(pending) => self.prepare_admission_commit(pending),
            SurfaceCommitEffect::AdmissionRepair(pending) => self.prepare_admission_repair(pending),
            SurfaceCommitEffect::AdmissionTerminal(pending) => {
                self.prepare_admission_terminal(pending)
            }
        }
    }

    #[cfg(test)]
    fn trace(&self) -> CommitControllerTrace {
        CommitControllerTrace {
            terminals: self.terminals.len(),
            pending_terminal_commits: self.pending_terminal_commits.len(),
            waiter_operations: self.terminal_waiters.len(),
            terminalization: self.terminalization.is_some(),
            admission_commits: self.admission_commits.len(),
            admission_repairs: self.admission_repairs.len(),
            admission_terminals: self.admission_terminals.len(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct CommitControllerTrace {
    terminals: usize,
    pending_terminal_commits: usize,
    waiter_operations: usize,
    terminalization: bool,
    admission_commits: usize,
    admission_repairs: usize,
    admission_terminals: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Pending {
        operation_id: surface::SurfaceOperationId,
        retry_at: Instant,
        goal_owned: bool,
        identity: u8,
    }

    impl ScheduledSurfaceCommit for Pending {
        fn operation_id(&self) -> &surface::SurfaceOperationId {
            &self.operation_id
        }
        fn retry_at(&self) -> Instant {
            self.retry_at
        }
        fn defer_until(&mut self, retry_at: Instant) {
            self.retry_at = retry_at;
        }
    }

    impl GoalRecoverySurfaceCommit for Pending {
        fn owns_goal_recovery(&self) -> bool {
            self.goal_owned
        }
    }

    fn pending(identity: u8, goal_owned: bool) -> Pending {
        Pending {
            operation_id: surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            retry_at: Instant::now(),
            goal_owned,
            identity,
        }
    }

    #[test]
    fn commit_controller_trace_equivalence() {
        let mut controller =
            SurfaceCommitController::<Pending, Pending, Pending, Pending, Pending>::new(
                HashMap::new(),
            );
        let terminalization = pending(1, false);
        let admission = pending(2, false);
        let repair = pending(3, true);
        let terminal = pending(4, true);
        let pending_terminal = pending(5, false);
        let pending_terminal_id = pending_terminal.operation_id.clone();
        let (waiter_tx, waiter_rx) = std::sync::mpsc::sync_channel(1);
        let mut trace = vec![controller.trace()];

        controller.retain_pending_terminal(pending_terminal_id.clone(), pending_terminal);
        controller.register_terminal_waiter(
            pending_terminal_id.clone(),
            waiter_tx,
            surface::OptionalProcessLocalCancel::new(),
        );
        let _initial_effect = controller
            .prepare_terminalization(terminalization.clone())
            .unwrap_or_else(|_| panic!("terminalization slot should be empty"));
        controller.prepare_admission_commit(admission.clone());
        controller.prepare_admission_repair(repair.clone());
        controller.prepare_admission_terminal(terminal.clone());
        trace.push(controller.trace());
        assert!(matches!(
            controller.next_retry(),
            Some((_, SurfaceCommitRetryKey::AdmissionCommit(_)))
                | Some((_, SurfaceCommitRetryKey::AdmissionRepair(_)))
                | Some((_, SurfaceCommitRetryKey::AdmissionTerminal(_)))
                | Some((_, SurfaceCommitRetryKey::Terminalization(_)))
        ));
        assert!(controller.has_goal_recovery_owner());
        assert_eq!(
            controller.goal_recovery_operation_id(),
            Some(repair.operation_id.clone())
        );
        assert!(controller.has_pending_terminal(&pending_terminal_id));
        assert_eq!(
            controller
                .pending_terminal(&pending_terminal_id)
                .map(|pending| pending.identity),
            Some(5)
        );

        let effects = controller.settle_terminal_waiters(
            &pending_terminal_id,
            Ok(surface::WaitOperationTerminalResult::UnknownOperation {
                operation_id: pending_terminal_id.clone(),
            }),
            true,
        );
        assert_eq!(effects.len(), 1);
        match effects.into_iter().next().unwrap() {
            RuntimeActorEffect::ReplyOperation {
                reply,
                result,
                nonblocking,
            } => {
                assert!(nonblocking);
                reply.send(result).unwrap();
            }
            _ => panic!("terminal waiter settlement must remain actor-applied"),
        }
        assert!(matches!(
            waiter_rx.recv().unwrap().unwrap(),
            surface::WaitOperationTerminalResult::UnknownOperation { operation_id }
                if operation_id == pending_terminal_id
        ));

        let retry_at = Instant::now() + std::time::Duration::from_secs(1);
        let effect = controller
            .begin_attempt(&SurfaceCommitRetryKey::Terminalization(
                terminalization.operation_id.clone(),
            ))
            .unwrap();
        assert_eq!(
            match &effect {
                SurfaceCommitEffect::Terminalization(p) => p.identity,
                _ => 0,
            },
            1
        );
        assert!(
            controller
                .resolve_attempt(effect, SurfaceCommitResolution::RetryAt(retry_at))
                .is_none()
        );
        let effect = controller
            .begin_attempt(&SurfaceCommitRetryKey::Terminalization(
                terminalization.operation_id.clone(),
            ))
            .unwrap();
        let committed = controller
            .resolve_attempt(effect, SurfaceCommitResolution::Committed)
            .unwrap();
        assert_eq!(
            committed.key(),
            SurfaceCommitRetryKey::Terminalization(terminalization.operation_id)
        );
        assert!(!controller.has_terminalization());

        for key in [
            SurfaceCommitRetryKey::AdmissionCommit(admission.operation_id),
            SurfaceCommitRetryKey::AdmissionRepair(repair.operation_id),
            SurfaceCommitRetryKey::AdmissionTerminal(terminal.operation_id),
        ] {
            let effect = controller.begin_attempt(&key).unwrap();
            let resolution = if matches!(key, SurfaceCommitRetryKey::AdmissionRepair(_)) {
                SurfaceCommitResolution::Aborted
            } else {
                SurfaceCommitResolution::Committed
            };
            controller.resolve_attempt(effect, resolution).unwrap();
        }
        trace.push(controller.trace());
        assert_eq!(
            trace,
            vec![
                CommitControllerTrace {
                    terminals: 0,
                    pending_terminal_commits: 0,
                    waiter_operations: 0,
                    terminalization: false,
                    admission_commits: 0,
                    admission_repairs: 0,
                    admission_terminals: 0
                },
                CommitControllerTrace {
                    terminals: 0,
                    pending_terminal_commits: 1,
                    waiter_operations: 1,
                    terminalization: true,
                    admission_commits: 1,
                    admission_repairs: 1,
                    admission_terminals: 1
                },
                CommitControllerTrace {
                    terminals: 0,
                    pending_terminal_commits: 1,
                    waiter_operations: 0,
                    terminalization: false,
                    admission_commits: 0,
                    admission_repairs: 0,
                    admission_terminals: 0
                },
            ]
        );
    }

    #[test]
    fn terminalization_slot_rejects_replacement_and_stale_keys() {
        let mut controller =
            SurfaceCommitController::<Pending, Pending, Pending, Pending, Pending>::new(
                HashMap::new(),
            );
        let first = pending(1, false);
        let second = pending(2, false);

        assert!(controller.prepare_terminalization(first.clone()).is_ok());
        let rejected = match controller.prepare_terminalization(second.clone()) {
            Ok(_) => panic!("occupied slot must reject replacement"),
            Err(rejected) => rejected,
        };
        assert_eq!(rejected.identity, second.identity);
        assert!(
            controller
                .begin_attempt(&SurfaceCommitRetryKey::Terminalization(
                    second.operation_id.clone(),
                ))
                .is_none()
        );
        assert_eq!(
            controller.inspect_terminalization(|pending| pending.identity),
            Some(first.identity)
        );
    }

    #[test]
    fn cancelled_terminal_waiter_is_retired() {
        let mut controller =
            SurfaceCommitController::<Pending, Pending, Pending, Pending, Pending>::new(
                HashMap::new(),
            );
        let operation_id =
            surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let (waiter_tx, waiter_rx) = std::sync::mpsc::sync_channel(1);
        let cancel = surface::OptionalProcessLocalCancel::new();
        cancel.cancel();
        controller.register_terminal_waiter(operation_id.clone(), waiter_tx, cancel);

        let effects = controller.cancelled_terminal_waiters();
        assert_eq!(controller.terminal_waiter_count(&operation_id), 0);
        assert_eq!(effects.len(), 1);
        match effects.into_iter().next().unwrap() {
            RuntimeActorEffect::ReplyOperation {
                reply,
                result,
                nonblocking,
            } => {
                assert!(nonblocking);
                reply.send(result).unwrap();
            }
            _ => panic!("canceled terminal wait must reply through the actor effect"),
        }
        assert!(matches!(
            waiter_rx.recv().unwrap().unwrap(),
            surface::WaitOperationTerminalResult::WaitCancelled { operation_id: id }
                if id == operation_id
        ));
    }
}
