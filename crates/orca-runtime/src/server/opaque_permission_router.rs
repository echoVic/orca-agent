use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::surface::{
    DeferredMutation, JSONL_LIVE_REQUEST_LIMIT, JSONL_REPAIR_AUTHORITY_LIMIT,
    JSONL_REQUEST_TOMBSTONE_LIMIT, JSONL_REQUEST_TOMBSTONE_TTL_MS, RuntimeSurfaceClientHandle,
    SurfaceConnectionId, SurfaceInteractionId, SurfaceInteractionKind,
};

use super::lock_error;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct JsonlRetirementSequence(u64);

impl JsonlRetirementSequence {
    #[cfg(test)]
    fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum JsonlRetiredRequestOwner {
    ThreadPermission,
    CommandExecPermission,
    DirectUserInput,
    DirectMcpElicitation,
}

impl JsonlRetiredRequestOwner {
    pub(super) const fn rank(self) -> u8 {
        match self {
            Self::ThreadPermission => 0,
            Self::CommandExecPermission => 1,
            Self::DirectUserInput => 2,
            Self::DirectMcpElicitation => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonlLiveAdmissionFailureReason {
    IngressClosed,
    LiveLimitReached,
    RepairAuthorityLimitReached,
    RetirementSequenceExhausted,
    OpaqueIdExhausted,
}

pub(super) struct JsonlRepairAuthorityPermit {
    permit_id: u64,
}

pub(super) struct JsonlLiveRequestAdmission {
    pub(super) opaque_request_id: String,
    pub(super) owner: JsonlRetiredRequestOwner,
    pub(super) retirement_sequence: JsonlRetirementSequence,
    repair_authority_permit: JsonlRepairAuthorityPermit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlOwnerSettlement {
    InteractionRecoveryRetained,
    CommandExecFailedBeforeExecution,
}

#[derive(Clone)]
pub(super) struct JsonlCommandExecPermissionRequest {
    pub(super) thread_id: String,
    pub(super) runtime_workspace_roots: Vec<PathBuf>,
    pub(super) command: Vec<String>,
    pub(super) command_is_argv: bool,
    pub(super) process_id: Option<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) env: crate::protocol::CommandEnvOverrides,
    pub(super) options: crate::protocol::CommandExecOptions,
    pub(super) terminal: crate::shell_session::ShellTerminalMode,
    pub(super) event_id: serde_json::Value,
}

#[derive(Clone)]
pub(super) enum JsonlPermissionRoute {
    Surface {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
        target: SurfaceInteractionKind,
        thread_id: String,
        runtime_workspace_roots: Vec<PathBuf>,
    },
    CommandExec {
        request: Box<JsonlCommandExecPermissionRequest>,
    },
}

impl JsonlPermissionRoute {
    pub(super) fn thread_id(&self) -> &str {
        match self {
            Self::Surface { thread_id, .. } => thread_id,
            Self::CommandExec { request } => &request.thread_id,
        }
    }

    pub(super) fn runtime_workspace_roots(&self) -> &[PathBuf] {
        match self {
            Self::Surface {
                runtime_workspace_roots,
                ..
            } => runtime_workspace_roots,
            Self::CommandExec { request } => &request.runtime_workspace_roots,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum JsonlRetiredRequestSettlement {
    PermissionCommitted {
        response_digest: JsonlResponseDigest,
    },
    DirectInteractionCommitted {
        response_digest: JsonlResponseDigest,
    },
    TransportRetired {
        owner_settlement: JsonlOwnerSettlement,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct JsonlResponseDigest([u8; 32]);

pub(super) fn jsonl_response_digest(value: &impl Serialize) -> io::Result<JsonlResponseDigest> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| io::Error::other(format!("encode JSONL response digest: {error}")))?;
    Ok(JsonlResponseDigest(Sha256::digest(encoded).into()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonlCommittedReplay {
    SameResponse,
    ConflictingResponse,
    NotCommitted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JsonlRequestTombstone {
    pub(super) opaque_request_id: String,
    pub(super) owner: JsonlRetiredRequestOwner,
    pub(super) settlement: JsonlRetiredRequestSettlement,
    retired_at_ms: u64,
    expires_at_ms: u64,
    pub(super) retirement_sequence: JsonlRetirementSequence,
}

#[derive(Clone)]
pub(super) struct JsonlConnectionAdmission {
    state: Arc<Mutex<JsonlConnectionAdmissionState>>,
    route_registration_gate: Arc<Mutex<()>>,
}

struct JsonlConnectionAdmissionState {
    _connection_id: SurfaceConnectionId,
    started_at: Instant,
    ingress_closed: bool,
    next_opaque_suffix: u64,
    next_retirement_sequence: u64,
    next_repair_permit: u64,
    live_count: u64,
    repair_authority_count: u64,
    used_opaque_ids: HashSet<String>,
    tombstones: HashMap<String, JsonlRequestTombstone>,
}

impl JsonlConnectionAdmission {
    #[cfg(test)]
    pub(super) fn new_ephemeral() -> Self {
        Self::new(
            SurfaceConnectionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated JSONL connection id is valid"),
        )
    }

    pub(super) fn new(connection_id: SurfaceConnectionId) -> Self {
        Self {
            state: Arc::new(Mutex::new(JsonlConnectionAdmissionState {
                _connection_id: connection_id,
                started_at: Instant::now(),
                ingress_closed: false,
                next_opaque_suffix: 0,
                next_retirement_sequence: 0,
                next_repair_permit: 0,
                live_count: 0,
                repair_authority_count: 0,
                used_opaque_ids: HashSet::new(),
                tombstones: HashMap::new(),
            })),
            route_registration_gate: Arc::new(Mutex::new(())),
        }
    }

    pub(super) fn with_route_registration_barrier<T>(
        &self,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let _gate = self.route_registration_gate.lock().map_err(lock_error)?;
        operation()
    }

    pub(super) fn register(
        &self,
        preferred_request_id: &str,
        owner: JsonlRetiredRequestOwner,
    ) -> Result<JsonlLiveRequestAdmission, JsonlLiveAdmissionFailureReason> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JsonlLiveAdmissionFailureReason::IngressClosed)?;
        let now_ms = elapsed_ms(state.started_at);
        expire_tombstones(&mut state, now_ms);
        if state.ingress_closed {
            return Err(JsonlLiveAdmissionFailureReason::IngressClosed);
        }
        if state.live_count >= JSONL_LIVE_REQUEST_LIMIT {
            return Err(JsonlLiveAdmissionFailureReason::LiveLimitReached);
        }
        if state.repair_authority_count >= JSONL_REPAIR_AUTHORITY_LIMIT {
            return Err(JsonlLiveAdmissionFailureReason::RepairAuthorityLimitReached);
        }
        let next_retirement_sequence = state
            .next_retirement_sequence
            .checked_add(1)
            .ok_or(JsonlLiveAdmissionFailureReason::RetirementSequenceExhausted)?;
        let retirement_sequence = state.next_retirement_sequence;
        let permit_id = state.next_repair_permit;
        let next_repair_permit = state
            .next_repair_permit
            .checked_add(1)
            .ok_or(JsonlLiveAdmissionFailureReason::RepairAuthorityLimitReached)?;
        let (opaque_request_id, next_opaque_suffix) =
            reserve_opaque_id(&state, preferred_request_id)?;
        state.next_retirement_sequence = next_retirement_sequence;
        state.next_repair_permit = next_repair_permit;
        state.next_opaque_suffix = next_opaque_suffix;
        state.used_opaque_ids.insert(opaque_request_id.clone());
        state.live_count += 1;
        state.repair_authority_count += 1;
        Ok(JsonlLiveRequestAdmission {
            opaque_request_id,
            owner,
            retirement_sequence: JsonlRetirementSequence(retirement_sequence),
            repair_authority_permit: JsonlRepairAuthorityPermit { permit_id },
        })
    }

    pub(super) fn retire(
        &self,
        admission: JsonlLiveRequestAdmission,
        settlement: JsonlRetiredRequestSettlement,
    ) -> io::Result<JsonlRequestTombstone> {
        let mut state = self.state.lock().map_err(lock_error)?;
        let now_ms = elapsed_ms(state.started_at);
        expire_tombstones(&mut state, now_ms);
        if state.live_count == 0 {
            return Err(io::Error::other("JSONL live request counter underflow"));
        }
        if state.repair_authority_count == 0 {
            return Err(io::Error::other("JSONL repair authority counter underflow"));
        }
        state.live_count -= 1;
        state.repair_authority_count -= 1;
        let _permit_id = admission.repair_authority_permit.permit_id;
        let tombstone = JsonlRequestTombstone {
            opaque_request_id: admission.opaque_request_id.clone(),
            owner: admission.owner,
            settlement,
            retired_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(JSONL_REQUEST_TOMBSTONE_TTL_MS),
            retirement_sequence: admission.retirement_sequence,
        };
        state
            .tombstones
            .insert(admission.opaque_request_id, tombstone.clone());
        evict_tombstones(&mut state);
        Ok(tombstone)
    }

    pub(super) fn tombstone(&self, request_id: &str) -> io::Result<Option<JsonlRequestTombstone>> {
        let mut state = self.state.lock().map_err(lock_error)?;
        let now_ms = elapsed_ms(state.started_at);
        expire_tombstones(&mut state, now_ms);
        Ok(state.tombstones.get(request_id).cloned())
    }

    pub(super) fn close_ingress(&self) -> io::Result<()> {
        self.state.lock().map_err(lock_error)?.ingress_closed = true;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (u64, u64, usize) {
        let state = self.state.lock().unwrap();
        (
            state.live_count,
            state.repair_authority_count,
            state.tombstones.len(),
        )
    }
}

fn reserve_opaque_id(
    state: &JsonlConnectionAdmissionState,
    preferred: &str,
) -> Result<(String, u64), JsonlLiveAdmissionFailureReason> {
    if !preferred.is_empty() && !state.used_opaque_ids.contains(preferred) {
        return Ok((preferred.to_string(), state.next_opaque_suffix));
    }
    let mut next_opaque_suffix = state.next_opaque_suffix;
    loop {
        let suffix = next_opaque_suffix;
        next_opaque_suffix = next_opaque_suffix
            .checked_add(1)
            .ok_or(JsonlLiveAdmissionFailureReason::OpaqueIdExhausted)?;
        let candidate = format!("{preferred}-{suffix}");
        if !state.used_opaque_ids.contains(&candidate) {
            return Ok((candidate, next_opaque_suffix));
        }
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn expire_tombstones(state: &mut JsonlConnectionAdmissionState, now_ms: u64) {
    state
        .tombstones
        .retain(|_, tombstone| tombstone.expires_at_ms > now_ms);
}

fn evict_tombstones(state: &mut JsonlConnectionAdmissionState) {
    let Ok(limit) = usize::try_from(JSONL_REQUEST_TOMBSTONE_LIMIT) else {
        return;
    };
    if state.tombstones.len() <= limit {
        return;
    }
    let mut order = state
        .tombstones
        .values()
        .map(|tombstone| {
            (
                tombstone.expires_at_ms,
                tombstone.retirement_sequence,
                tombstone.owner.rank(),
                tombstone.opaque_request_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    order.sort();
    for (_, _, _, request_id) in order
        .into_iter()
        .take(state.tombstones.len().saturating_sub(limit))
    {
        state.tombstones.remove(&request_id);
    }
}

pub(super) struct JsonlOpaquePermissionRouter<T> {
    admission: JsonlConnectionAdmission,
    routes: Arc<Mutex<HashMap<String, JsonlOpaquePermissionEntry<T>>>>,
}

impl<T> Clone for JsonlOpaquePermissionRouter<T> {
    fn clone(&self) -> Self {
        Self {
            admission: self.admission.clone(),
            routes: Arc::clone(&self.routes),
        }
    }
}

struct JsonlOpaquePermissionEntry<T> {
    admission: Option<JsonlLiveRequestAdmission>,
    publication: JsonlPermissionPublicationState,
    state: JsonlPermissionRouteState,
    route: T,
}

#[derive(Clone, Copy)]
enum JsonlPermissionPublicationState {
    Registered,
    Writing { frame_digest: JsonlResponseDigest },
    Published { frame_digest: JsonlResponseDigest },
}

#[derive(Clone)]
enum JsonlPermissionRouteState {
    Routed,
    CommittedPending {
        request_id: crate::surface::SurfaceRequestId,
        commit_id: crate::surface::SurfaceCommitId,
        response_digest: JsonlResponseDigest,
    },
}

impl<T: Clone> JsonlOpaquePermissionRouter<T> {
    pub(super) fn has_live_routes(&self) -> bool {
        self.routes
            .lock()
            .map(|routes| !routes.is_empty())
            .unwrap_or(true)
    }

    pub(super) fn new(admission: JsonlConnectionAdmission) -> Self {
        Self {
            admission,
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn register(
        &self,
        preferred_request_id: String,
        owner: JsonlRetiredRequestOwner,
        route: T,
    ) -> io::Result<String> {
        self.admission.with_route_registration_barrier(|| {
            let admission = self
                .admission
                .register(&preferred_request_id, owner)
                .map_err(|reason| {
                    io::Error::other(format!("JSONL permission admission failed: {reason:?}"))
                })?;
            let request_id = admission.opaque_request_id.clone();
            let mut routes = self.routes.lock().map_err(lock_error)?;
            if routes
                .insert(
                    request_id.clone(),
                    JsonlOpaquePermissionEntry {
                        admission: Some(admission),
                        publication: JsonlPermissionPublicationState::Registered,
                        state: JsonlPermissionRouteState::Routed,
                        route,
                    },
                )
                .is_some()
            {
                return Err(io::Error::other("JSONL permission route collision"));
            }
            Ok(request_id)
        })
    }

    pub(super) fn published_route(&self, request_id: &str) -> io::Result<Option<T>> {
        if self.admission.tombstone(request_id)?.is_some() {
            return Ok(None);
        }
        Ok(self
            .routes
            .lock()
            .map_err(lock_error)?
            .get(request_id)
            .filter(|entry| {
                matches!(
                    entry.publication,
                    JsonlPermissionPublicationState::Published { .. }
                )
            })
            .map(|entry| entry.route.clone()))
    }

    pub(super) fn mark_writing(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL permission route is no longer live"))?;
        match entry.publication {
            JsonlPermissionPublicationState::Registered => {
                entry.publication = JsonlPermissionPublicationState::Writing { frame_digest };
                Ok(())
            }
            JsonlPermissionPublicationState::Writing {
                frame_digest: existing,
            } if existing == frame_digest => Ok(()),
            JsonlPermissionPublicationState::Writing { .. }
            | JsonlPermissionPublicationState::Published { .. } => Err(io::Error::other(
                "JSONL permission frame entered writing with a different digest",
            )),
        }
    }

    pub(super) fn mark_published(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL permission route is no longer live"))?;
        match entry.publication {
            JsonlPermissionPublicationState::Writing {
                frame_digest: existing,
            } if existing == frame_digest => {
                entry.publication = JsonlPermissionPublicationState::Published { frame_digest };
                Ok(())
            }
            JsonlPermissionPublicationState::Published {
                frame_digest: existing,
            } if existing == frame_digest => Ok(()),
            JsonlPermissionPublicationState::Registered
            | JsonlPermissionPublicationState::Writing { .. }
            | JsonlPermissionPublicationState::Published { .. } => Err(io::Error::other(
                "JSONL permission frame publication has no matching writing witness",
            )),
        }
    }

    pub(super) fn settle(
        &self,
        request_id: &str,
        settlement: JsonlRetiredRequestSettlement,
    ) -> io::Result<Option<JsonlRequestTombstone>> {
        let entry = self.routes.lock().map_err(lock_error)?.remove(request_id);
        let Some(mut entry) = entry else {
            return Ok(self.admission.tombstone(request_id)?);
        };
        let admission = entry
            .admission
            .take()
            .ok_or_else(|| io::Error::other("JSONL permission admission already consumed"))?;
        self.admission.retire(admission, settlement).map(Some)
    }

    pub(super) fn mark_committed_pending(
        &self,
        request_id: &str,
        mutation: &DeferredMutation,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.mark_committed_pending_witness(
            request_id,
            mutation.request_id.clone(),
            mutation.commit_id.clone(),
            response_digest,
        )
    }

    fn mark_committed_pending_witness(
        &self,
        request_id: &str,
        mutation_request_id: crate::surface::SurfaceRequestId,
        commit_id: crate::surface::SurfaceCommitId,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL permission route is no longer live"))?;
        match &entry.state {
            JsonlPermissionRouteState::Routed => {
                entry.state = JsonlPermissionRouteState::CommittedPending {
                    request_id: mutation_request_id,
                    commit_id,
                    response_digest,
                };
                Ok(())
            }
            JsonlPermissionRouteState::CommittedPending {
                request_id: existing_request_id,
                commit_id: existing_commit_id,
                response_digest: existing_response_digest,
            } if existing_request_id == &mutation_request_id
                && existing_commit_id == &commit_id
                && existing_response_digest == &response_digest =>
            {
                Ok(())
            }
            JsonlPermissionRouteState::CommittedPending { .. } => Err(io::Error::other(
                "JSONL permission route has a different committed repair witness",
            )),
        }
    }

    pub(super) fn committed_replay(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<JsonlCommittedReplay> {
        let Some(tombstone) = self.admission.tombstone(request_id)? else {
            return Ok(JsonlCommittedReplay::NotCommitted);
        };
        let committed_digest = match tombstone.settlement {
            JsonlRetiredRequestSettlement::PermissionCommitted { response_digest } => {
                response_digest
            }
            JsonlRetiredRequestSettlement::DirectInteractionCommitted { .. }
            | JsonlRetiredRequestSettlement::TransportRetired { .. } => {
                return Ok(JsonlCommittedReplay::NotCommitted);
            }
        };
        Ok(if committed_digest == response_digest {
            JsonlCommittedReplay::SameResponse
        } else {
            JsonlCommittedReplay::ConflictingResponse
        })
    }

    pub(super) fn close_routes_by_owner(&self) -> io::Result<Vec<JsonlRequestTombstone>> {
        let routes = self.routes.lock().map_err(lock_error)?;
        let request_ids = routes
            .iter()
            .filter(|(_, entry)| matches!(entry.state, JsonlPermissionRouteState::Routed))
            .map(|(request_id, entry)| {
                let owner = entry
                    .admission
                    .as_ref()
                    .map(|admission| admission.owner)
                    .ok_or_else(|| {
                        io::Error::other("JSONL permission admission already consumed")
                    })?;
                Ok((request_id.clone(), owner))
            })
            .collect::<io::Result<Vec<_>>>()?;
        drop(routes);
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for (request_id, owner) in request_ids {
            let owner_settlement = match owner {
                JsonlRetiredRequestOwner::CommandExecPermission => {
                    JsonlOwnerSettlement::CommandExecFailedBeforeExecution
                }
                JsonlRetiredRequestOwner::ThreadPermission => {
                    JsonlOwnerSettlement::InteractionRecoveryRetained
                }
                JsonlRetiredRequestOwner::DirectUserInput
                | JsonlRetiredRequestOwner::DirectMcpElicitation => {
                    return Err(io::Error::other(
                        "direct interaction route stored in permission router",
                    ));
                }
            };
            if let Some(tombstone) = self.settle(
                &request_id,
                JsonlRetiredRequestSettlement::TransportRetired { owner_settlement },
            )? {
                tombstones.push(tombstone);
            }
        }
        Ok(tombstones)
    }

    pub(super) fn settle_committed_pending(&self) -> io::Result<Vec<JsonlRequestTombstone>> {
        let request_ids = self
            .routes
            .lock()
            .map_err(lock_error)?
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.state,
                    JsonlPermissionRouteState::CommittedPending { .. }
                )
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for request_id in request_ids {
            let response_digest = {
                let routes = self.routes.lock().map_err(lock_error)?;
                let entry = routes.get(&request_id).ok_or_else(|| {
                    io::Error::other("JSONL committed permission route disappeared")
                })?;
                match entry.state {
                    JsonlPermissionRouteState::CommittedPending {
                        response_digest, ..
                    } => response_digest,
                    JsonlPermissionRouteState::Routed => {
                        return Err(io::Error::other(
                            "JSONL routed permission entered committed settlement",
                        ));
                    }
                }
            };
            if let Some(tombstone) = self.settle(
                &request_id,
                JsonlRetiredRequestSettlement::PermissionCommitted { response_digest },
            )? {
                tombstones.push(tombstone);
            }
        }
        Ok(tombstones)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_id() -> SurfaceConnectionId {
        SurfaceConnectionId::try_from_bytes([
            1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 245,
        ])
        .unwrap()
    }

    #[test]
    fn owner_rank_is_closed_and_stable() {
        assert_eq!(JsonlRetiredRequestOwner::ThreadPermission.rank(), 0);
        assert_eq!(JsonlRetiredRequestOwner::CommandExecPermission.rank(), 1);
        assert_eq!(JsonlRetiredRequestOwner::DirectUserInput.rank(), 2);
        assert_eq!(JsonlRetiredRequestOwner::DirectMcpElicitation.rank(), 3);
    }

    #[test]
    fn admission_shares_live_and_repair_capacity_and_never_reuses_ids() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        let first = admission
            .register("request", JsonlRetiredRequestOwner::ThreadPermission)
            .unwrap();
        let second = admission
            .register("request", JsonlRetiredRequestOwner::DirectUserInput)
            .unwrap();
        assert_eq!(first.opaque_request_id, "request");
        assert_ne!(second.opaque_request_id, "request");
        assert_eq!(first.retirement_sequence.get(), 0);
        assert_eq!(second.retirement_sequence.get(), 1);
        assert_eq!(admission.counts(), (2, 2, 0));

        admission
            .retire(
                first,
                JsonlRetiredRequestSettlement::PermissionCommitted {
                    response_digest: jsonl_response_digest(&"first").unwrap(),
                },
            )
            .unwrap();
        assert_eq!(admission.counts(), (1, 1, 1));
        let third = admission
            .register("request", JsonlRetiredRequestOwner::DirectMcpElicitation)
            .unwrap();
        assert_ne!(third.opaque_request_id, "request");
        assert_eq!(third.retirement_sequence.get(), 2);
    }

    #[test]
    fn closing_ingress_rejects_new_routes_without_changing_existing_counts() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        let _live = admission
            .register("live", JsonlRetiredRequestOwner::ThreadPermission)
            .unwrap();
        admission.close_ingress().unwrap();
        assert_eq!(
            admission
                .register("late", JsonlRetiredRequestOwner::DirectUserInput)
                .err(),
            Some(JsonlLiveAdmissionFailureReason::IngressClosed)
        );
        assert_eq!(admission.counts(), (1, 1, 0));
    }

    #[test]
    fn exhausted_retirement_sequence_does_not_partially_register() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        {
            let mut state = admission.state.lock().unwrap();
            state.next_retirement_sequence = u64::MAX;
        }
        assert_eq!(
            admission
                .register("request", JsonlRetiredRequestOwner::ThreadPermission)
                .err(),
            Some(JsonlLiveAdmissionFailureReason::RetirementSequenceExhausted)
        );
        let state = admission.state.lock().unwrap();
        assert_eq!(state.next_opaque_suffix, 0);
        assert_eq!(state.next_repair_permit, 0);
        assert_eq!(state.live_count, 0);
        assert_eq!(state.repair_authority_count, 0);
        assert!(state.used_opaque_ids.is_empty());
    }

    #[test]
    fn shared_live_limit_rejects_without_mutating_existing_admissions() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        let mut live = Vec::new();
        for index in 0..JSONL_LIVE_REQUEST_LIMIT {
            let owner = if index % 2 == 0 {
                JsonlRetiredRequestOwner::ThreadPermission
            } else {
                JsonlRetiredRequestOwner::DirectUserInput
            };
            live.push(
                admission
                    .register(&format!("request-{index}"), owner)
                    .unwrap(),
            );
        }
        assert_eq!(
            admission
                .register("overflow", JsonlRetiredRequestOwner::DirectMcpElicitation)
                .err(),
            Some(JsonlLiveAdmissionFailureReason::LiveLimitReached)
        );
        assert_eq!(
            admission.counts(),
            (JSONL_LIVE_REQUEST_LIMIT, JSONL_REPAIR_AUTHORITY_LIMIT, 0)
        );
        assert_eq!(live.len(), JSONL_LIVE_REQUEST_LIMIT as usize);
    }

    #[test]
    fn retirement_counter_validation_is_atomic() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        let live = admission
            .register("request", JsonlRetiredRequestOwner::ThreadPermission)
            .unwrap();
        admission.state.lock().unwrap().repair_authority_count = 0;

        assert!(
            admission
                .retire(
                    live,
                    JsonlRetiredRequestSettlement::TransportRetired {
                        owner_settlement: JsonlOwnerSettlement::InteractionRecoveryRetained,
                    },
                )
                .is_err()
        );
        assert_eq!(admission.counts(), (1, 0, 0));
    }

    #[test]
    fn committed_pending_permission_is_never_transport_retired() {
        let admission = JsonlConnectionAdmission::new(connection_id());
        let router = JsonlOpaquePermissionRouter::new(admission);
        router
            .register(
                "permission".to_string(),
                JsonlRetiredRequestOwner::ThreadPermission,
                "route".to_string(),
            )
            .unwrap();
        router
            .mark_committed_pending_witness(
                "permission",
                crate::surface::SurfaceRequestId::new(),
                crate::surface::SurfaceCommitId::try_from_bytes([
                    1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 246,
                ])
                .unwrap(),
                jsonl_response_digest(&"allow").unwrap(),
            )
            .unwrap();

        assert!(router.close_routes_by_owner().unwrap().is_empty());
        let tombstones = router.settle_committed_pending().unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(
            tombstones[0].settlement,
            JsonlRetiredRequestSettlement::PermissionCommitted {
                response_digest: jsonl_response_digest(&"allow").unwrap(),
            }
        );
        assert_eq!(
            router
                .committed_replay("permission", jsonl_response_digest(&"allow").unwrap())
                .unwrap(),
            JsonlCommittedReplay::SameResponse
        );
        assert_eq!(
            router
                .committed_replay("permission", jsonl_response_digest(&"deny").unwrap())
                .unwrap(),
            JsonlCommittedReplay::ConflictingResponse
        );
    }

    #[test]
    fn permission_route_is_releasable_only_after_matching_physical_publication() {
        let router =
            JsonlOpaquePermissionRouter::new(JsonlConnectionAdmission::new(connection_id()));
        router
            .register(
                "permission".to_string(),
                JsonlRetiredRequestOwner::ThreadPermission,
                "route".to_string(),
            )
            .unwrap();
        let digest = jsonl_response_digest(&"frame").unwrap();
        let other = jsonl_response_digest(&"other").unwrap();
        assert!(router.published_route("permission").unwrap().is_none());
        router.mark_writing("permission", digest).unwrap();
        assert!(router.published_route("permission").unwrap().is_none());
        assert!(router.mark_published("permission", other).is_err());
        router.mark_published("permission", digest).unwrap();
        assert_eq!(
            router.published_route("permission").unwrap().as_deref(),
            Some("route")
        );
    }
}
