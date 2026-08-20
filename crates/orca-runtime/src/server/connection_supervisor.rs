use std::io;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::surface::{JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS, JSONL_SUPERVISOR_JOIN_DEADLINE_MS};

use super::JsonlSurfaceAdapter;
use super::command_exec_manager::CommandExecManager;
use super::direct_interaction_adapter::{
    JsonlDirectInteractionAdapter, JsonlDirectInteractionRoute,
};
use super::fuzzy_file_search_manager::FuzzyFileSearchManager;
use super::mention_search_manager::MentionSearchManager;
use super::opaque_permission_router::{
    JsonlConnectionAdmission, JsonlOpaquePermissionRouter, JsonlOwnerSettlement,
    JsonlPermissionRoute,
};
use super::shell_manager::ServerShellManager;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlNonIoCloseTrigger {
    EndOfFile,
    #[cfg(test)]
    SupervisorShutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlSupervisorIoFailure {
    ReadFailed(String),
    WriteFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlSupervisorCloseTrigger {
    NonIo(JsonlNonIoCloseTrigger),
    Io(JsonlSupervisorIoFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonlServiceSettlementState {
    Joined,
    CleanupUnconfirmed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JsonlServiceSettlements {
    pub(super) command_exec: JsonlServiceSettlementState,
    pub(super) shell: JsonlServiceSettlementState,
    pub(super) file_search: JsonlServiceSettlementState,
    pub(super) mention_search: JsonlServiceSettlementState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlCommittedRepairSettlement {
    Completed { retired: usize },
    DeadlineRetained,
    FailedRetained { error: String },
}

impl JsonlCommittedRepairSettlement {
    fn is_healthy(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JsonlCommittedRepairSettlements {
    pub(super) permission: JsonlCommittedRepairSettlement,
    pub(super) direct_interaction: JsonlCommittedRepairSettlement,
}

impl JsonlCommittedRepairSettlements {
    fn is_healthy(&self) -> bool {
        self.permission.is_healthy() && self.direct_interaction.is_healthy()
    }
}

impl JsonlServiceSettlements {
    fn is_healthy(&self) -> bool {
        self.command_exec == JsonlServiceSettlementState::Joined
            && self.shell == JsonlServiceSettlementState::Joined
            && self.file_search == JsonlServiceSettlementState::Joined
            && self.mention_search == JsonlServiceSettlementState::Joined
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JsonlSupervisorCloseEvidence {
    pub(super) trigger: JsonlSupervisorCloseTrigger,
    pub(super) services: JsonlServiceSettlements,
    pub(super) repairs: JsonlCommittedRepairSettlements,
    pub(super) cleanup_errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlSupervisorCloseResult {
    Clean(JsonlSupervisorCloseEvidence),
    CleanupDegraded(JsonlSupervisorCloseEvidence),
    ShutdownFailed {
        error: String,
        evidence: JsonlSupervisorCloseEvidence,
    },
    IoFailed {
        shutdown_health: Option<String>,
        evidence: JsonlSupervisorCloseEvidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonlSupervisorState {
    Open,
    IngressClosed,
    RoutesRetired,
    ServicesSettled,
    RuntimeShutdownPending,
    Closed,
}

pub(super) struct JsonlConnectionSupervisor {
    state: JsonlSupervisorState,
    admission: JsonlConnectionAdmission,
    permission_routes: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
    direct_routes: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
}

pub(super) struct JsonlConnectionServices {
    pub(super) threads: JsonlSurfaceAdapter,
    pub(super) shells: ServerShellManager,
    pub(super) command_exec: CommandExecManager,
    pub(super) fuzzy_file_searches: FuzzyFileSearchManager,
    pub(super) mention_searches: MentionSearchManager,
}

impl JsonlConnectionSupervisor {
    pub(super) fn new(
        admission: JsonlConnectionAdmission,
        permission_routes: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
        direct_routes: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
    ) -> Self {
        Self {
            state: JsonlSupervisorState::Open,
            admission,
            permission_routes,
            direct_routes,
        }
    }

    pub(super) fn close(
        mut self,
        trigger: JsonlSupervisorCloseTrigger,
        services: JsonlConnectionServices,
    ) -> JsonlSupervisorCloseResult {
        self.state = JsonlSupervisorState::IngressClosed;
        let mut cleanup_errors = Vec::new();

        let JsonlConnectionServices {
            mut threads,
            shells,
            command_exec,
            fuzzy_file_searches,
            mention_searches,
        } = services;

        let clean_eof = matches!(
            &trigger,
            JsonlSupervisorCloseTrigger::NonIo(JsonlNonIoCloseTrigger::EndOfFile)
        );
        let mut had_permission_routes = true;
        let mut direct_routes_settled = false;
        if let Err(error) = self.admission.with_route_registration_barrier(|| {
            self.admission.close_ingress()?;
            had_permission_routes = self.permission_routes.has_live_routes();
            if let Err(error) = self.permission_routes.close_routes_by_owner() {
                cleanup_errors.push(format!("retire permission routes: {error}"));
            }
            direct_routes_settled = true;
            if clean_eof {
                let direct_settlement = self.direct_routes.settle_unreachable_routes();
                if !direct_settlement.is_complete() {
                    cleanup_errors.push(format!(
                        "unresolved direct interaction routes at clean EOF: {}",
                        direct_settlement.describe()
                    ));
                    direct_routes_settled = false;
                }
            }
            if let Err(error) = self
                .direct_routes
                .close_routes(JsonlOwnerSettlement::InteractionRecoveryRetained)
            {
                cleanup_errors.push(format!("retire direct interaction routes: {error}"));
            }
            Ok(())
        }) {
            cleanup_errors.push(format!(
                "close JSONL ingress and route registration: {error}"
            ));
        }
        let repair_deadline =
            Instant::now() + Duration::from_millis(JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS);
        let repairs = settle_committed_repairs_until(
            self.permission_routes.clone(),
            self.direct_routes.clone(),
            repair_deadline,
        );
        self.state = JsonlSupervisorState::RoutesRetired;

        let direct_repairs_healthy = repairs.direct_interaction.is_healthy();
        let wait_clean_eof_one_shots = should_wait_clean_eof_one_shots(
            &trigger,
            had_permission_routes,
            direct_routes_settled,
            direct_repairs_healthy,
        );
        if clean_eof && !wait_clean_eof_one_shots {
            if had_permission_routes {
                cleanup_errors.push(
                    "skip clean EOF one-shot completion while permission routes remain unresolved"
                        .to_string(),
                );
            }
            if !direct_repairs_healthy {
                cleanup_errors.push(
                    "skip clean EOF one-shot completion after direct repair settlement degraded"
                        .to_string(),
                );
            }
        }
        if wait_clean_eof_one_shots {
            if let Err(error) = threads.wait_clean_eof_one_shots() {
                cleanup_errors.push(format!("wait clean EOF one-shot terminals: {error}"));
            }
        }

        let service_deadline =
            Instant::now() + Duration::from_millis(JSONL_SUPERVISOR_JOIN_DEADLINE_MS);
        let service_settlements = settle_services(
            command_exec,
            shells,
            fuzzy_file_searches,
            mention_searches,
            service_deadline,
        );
        self.state = JsonlSupervisorState::ServicesSettled;

        let evidence = JsonlSupervisorCloseEvidence {
            trigger: trigger.clone(),
            services: service_settlements,
            repairs,
            cleanup_errors,
        };
        self.state = JsonlSupervisorState::RuntimeShutdownPending;
        let shutdown = threads.shutdown();
        self.state = JsonlSupervisorState::Closed;

        match trigger {
            JsonlSupervisorCloseTrigger::Io(_) => JsonlSupervisorCloseResult::IoFailed {
                shutdown_health: shutdown.err().map(|error| error.to_string()),
                evidence,
            },
            JsonlSupervisorCloseTrigger::NonIo(_) => match shutdown {
                Err(error) => JsonlSupervisorCloseResult::ShutdownFailed {
                    error: error.to_string(),
                    evidence,
                },
                Ok(())
                    if evidence.services.is_healthy()
                        && evidence.repairs.is_healthy()
                        && evidence.cleanup_errors.is_empty() =>
                {
                    JsonlSupervisorCloseResult::Clean(evidence)
                }
                Ok(()) => JsonlSupervisorCloseResult::CleanupDegraded(evidence),
            },
        }
    }
}

fn should_wait_clean_eof_one_shots(
    trigger: &JsonlSupervisorCloseTrigger,
    had_permission_routes: bool,
    direct_routes_settled: bool,
    direct_repairs_healthy: bool,
) -> bool {
    matches!(
        trigger,
        JsonlSupervisorCloseTrigger::NonIo(JsonlNonIoCloseTrigger::EndOfFile)
    ) && !had_permission_routes
        && direct_routes_settled
        && direct_repairs_healthy
}

fn settle_services(
    mut command_exec: CommandExecManager,
    mut shells: ServerShellManager,
    fuzzy_file_searches: FuzzyFileSearchManager,
    mention_searches: MentionSearchManager,
    deadline: Instant,
) -> JsonlServiceSettlements {
    let command_and_shell = thread::Builder::new()
        .name("orca-jsonl-command-shell-settlement".to_string())
        .spawn(move || {
            command_exec.terminate_all(shells.sessions_mut());
            shells.terminate_all();
            (
                JsonlServiceSettlementState::Joined,
                JsonlServiceSettlementState::Joined,
            )
        })
        .ok();
    let file_search = spawn_service_settlement("file-search", move || {
        fuzzy_file_searches.settle_until(deadline)
    });
    let mention_search = spawn_service_settlement("mention-search", move || {
        mention_searches.settle_until(deadline)
    });

    let command_and_shell = join_until(command_and_shell, deadline).unwrap_or((
        JsonlServiceSettlementState::CleanupUnconfirmed,
        JsonlServiceSettlementState::CleanupUnconfirmed,
    ));
    JsonlServiceSettlements {
        command_exec: command_and_shell.0,
        shell: command_and_shell.1,
        file_search: join_until(file_search, deadline)
            .unwrap_or(JsonlServiceSettlementState::CleanupUnconfirmed),
        mention_search: join_until(mention_search, deadline)
            .unwrap_or(JsonlServiceSettlementState::CleanupUnconfirmed),
    }
}

fn settle_committed_repairs_until(
    permission_routes: JsonlOpaquePermissionRouter<JsonlPermissionRoute>,
    direct_routes: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
    deadline: Instant,
) -> JsonlCommittedRepairSettlements {
    let permission = thread::Builder::new()
        .name("orca-jsonl-permission-repair-settlement".to_string())
        .spawn(move || permission_routes.settle_committed_pending());
    let direct = thread::Builder::new()
        .name("orca-jsonl-direct-repair-settlement".to_string())
        .spawn(move || direct_routes.settle_committed_pending());

    JsonlCommittedRepairSettlements {
        permission: join_repair_until(permission, deadline),
        direct_interaction: join_repair_until(direct, deadline),
    }
}

fn join_repair_until<T>(
    handle: io::Result<JoinHandle<io::Result<Vec<T>>>>,
    deadline: Instant,
) -> JsonlCommittedRepairSettlement {
    let handle = match handle {
        Ok(handle) => handle,
        Err(error) => {
            return JsonlCommittedRepairSettlement::FailedRetained {
                error: error.to_string(),
            };
        }
    };
    match join_until(Some(handle), deadline) {
        None => JsonlCommittedRepairSettlement::DeadlineRetained,
        Some(Ok(tombstones)) => JsonlCommittedRepairSettlement::Completed {
            retired: tombstones.len(),
        },
        Some(Err(error)) => JsonlCommittedRepairSettlement::FailedRetained {
            error: error.to_string(),
        },
    }
}

fn spawn_service_settlement<T: Send + 'static>(
    name: &str,
    settle: impl FnOnce() -> T + Send + 'static,
) -> Option<JoinHandle<T>> {
    thread::Builder::new()
        .name(format!("orca-jsonl-{name}-settlement"))
        .spawn(settle)
        .ok()
}

fn join_until<T>(handle: Option<JoinHandle<T>>, deadline: Instant) -> Option<T> {
    let handle = handle?;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    handle.join().ok()
}

impl JsonlSupervisorCloseResult {
    pub(super) fn into_io_result(self) -> io::Result<()> {
        match self {
            Self::Clean(_) | Self::CleanupDegraded(_) | Self::IoFailed { .. } => Ok(()),
            Self::ShutdownFailed { error, .. } => Err(io::Error::other(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_eof_wait_requires_no_permission_routes_and_a_healthy_direct_repair() {
        let eof = JsonlSupervisorCloseTrigger::NonIo(JsonlNonIoCloseTrigger::EndOfFile);
        assert!(should_wait_clean_eof_one_shots(&eof, false, true, true));
        assert!(!should_wait_clean_eof_one_shots(&eof, true, true, true));
        assert!(!should_wait_clean_eof_one_shots(&eof, false, false, true));
        assert!(!should_wait_clean_eof_one_shots(&eof, false, true, false));
        assert!(!should_wait_clean_eof_one_shots(
            &JsonlSupervisorCloseTrigger::Io(JsonlSupervisorIoFailure::WriteFailed(
                "shutdown".to_string(),
            )),
            false,
            true,
            true,
        ));
    }

    #[test]
    fn four_fixed_service_fields_are_required_for_clean_close() {
        let healthy = JsonlServiceSettlements {
            command_exec: JsonlServiceSettlementState::Joined,
            shell: JsonlServiceSettlementState::Joined,
            file_search: JsonlServiceSettlementState::Joined,
            mention_search: JsonlServiceSettlementState::Joined,
        };
        assert!(healthy.is_healthy());
        let mut degraded = healthy;
        degraded.mention_search = JsonlServiceSettlementState::CleanupUnconfirmed;
        assert!(!degraded.is_healthy());
    }

    #[test]
    fn committed_repair_evidence_distinguishes_completion_deadline_and_failure() {
        assert!(JsonlCommittedRepairSettlement::Completed { retired: 0 }.is_healthy());
        assert!(!JsonlCommittedRepairSettlement::DeadlineRetained.is_healthy());
        assert!(
            !JsonlCommittedRepairSettlement::FailedRetained {
                error: "poisoned route".to_string()
            }
            .is_healthy()
        );

        let deadline = join_repair_until(
            Ok(thread::spawn(|| -> io::Result<Vec<()>> {
                thread::sleep(Duration::from_millis(20));
                Ok(Vec::new())
            })),
            Instant::now(),
        );
        assert_eq!(deadline, JsonlCommittedRepairSettlement::DeadlineRetained);

        let failed =
            thread::spawn(|| -> io::Result<Vec<()>> { Err(io::Error::other("repair failed")) });
        assert_eq!(
            join_repair_until(Ok(failed), Instant::now() + Duration::from_secs(1)),
            JsonlCommittedRepairSettlement::FailedRetained {
                error: "repair failed".to_string()
            }
        );
    }
}
