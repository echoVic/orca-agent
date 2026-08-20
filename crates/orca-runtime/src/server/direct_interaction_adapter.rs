use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::lock_error;
use super::opaque_permission_router::{
    JsonlCommittedReplay, JsonlConnectionAdmission, JsonlLiveRequestAdmission,
    JsonlRequestTombstone, JsonlResponseDigest, JsonlRetiredRequestOwner,
    JsonlRetiredRequestSettlement, jsonl_response_digest,
};
use crate::surface::{
    DeferredMutation, MutationReply, RuntimeSurfaceClientHandle, SurfaceClientInteractionAnswer,
    SurfaceInteractionId, SurfaceMcpElicitationDecision, SurfaceRequestId,
    SurfaceUserInputDecision,
};

#[derive(Clone)]
pub(super) enum JsonlDirectInteractionRoute {
    UserInput {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
    },
    McpElicitation {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JsonlDirectInteractionKind {
    UserInput,
    McpElicitation,
}

#[derive(Clone)]
pub(super) struct JsonlDirectInteractionAdapter<T> {
    admission: JsonlConnectionAdmission,
    routes: Arc<Mutex<HashMap<String, JsonlDirectInteractionEntry<T>>>>,
}

struct JsonlDirectInteractionEntry<T> {
    admission: Option<JsonlLiveRequestAdmission>,
    kind: JsonlDirectInteractionKind,
    publication: JsonlDirectPublicationState,
    state: JsonlDirectInteractionState,
    route: T,
}

#[derive(Clone, Copy)]
enum JsonlDirectPublicationState {
    Registered,
    Writing,
    Published,
}

#[derive(Clone)]
enum JsonlDirectInteractionState {
    Routed,
    CommittedPending {
        request_id: crate::surface::SurfaceRequestId,
        commit_id: crate::surface::SurfaceCommitId,
        response_digest: JsonlResponseDigest,
    },
}

impl<T: Clone> JsonlDirectInteractionAdapter<T> {
    pub(super) fn new(admission: JsonlConnectionAdmission) -> Self {
        Self {
            admission,
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn register(
        &self,
        preferred_request_id: String,
        kind: JsonlDirectInteractionKind,
        route: T,
    ) -> io::Result<String> {
        let owner = match kind {
            JsonlDirectInteractionKind::UserInput => JsonlRetiredRequestOwner::DirectUserInput,
            JsonlDirectInteractionKind::McpElicitation => {
                JsonlRetiredRequestOwner::DirectMcpElicitation
            }
        };
        self.admission.with_route_registration_barrier(|| {
            let admission = self
                .admission
                .register(&preferred_request_id, owner)
                .map_err(|reason| {
                    io::Error::other(format!(
                        "JSONL direct interaction admission failed: {reason:?}"
                    ))
                })?;
            let request_id = admission.opaque_request_id.clone();
            let mut routes = self.routes.lock().map_err(lock_error)?;
            if routes
                .insert(
                    request_id.clone(),
                    JsonlDirectInteractionEntry {
                        admission: Some(admission),
                        kind,
                        publication: JsonlDirectPublicationState::Registered,
                        state: JsonlDirectInteractionState::Routed,
                        route,
                    },
                )
                .is_some()
            {
                return Err(io::Error::other("JSONL direct interaction route collision"));
            }
            Ok(request_id)
        })
    }

    pub(super) fn published_route(
        &self,
        request_id: &str,
        expected_kind: JsonlDirectInteractionKind,
    ) -> io::Result<Option<T>> {
        if self.admission.tombstone(request_id)?.is_some() {
            return Ok(None);
        }
        Ok(self
            .routes
            .lock()
            .map_err(lock_error)?
            .get(request_id)
            .filter(|entry| {
                entry.kind == expected_kind
                    && matches!(entry.publication, JsonlDirectPublicationState::Published)
            })
            .map(|entry| entry.route.clone()))
    }

    pub(super) fn publish(
        &self,
        request_id: &str,
        write_frame: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let mut routes = self.routes.lock().map_err(lock_error)?;
        let entry = routes
            .get_mut(request_id)
            .ok_or_else(|| io::Error::other("JSONL direct route is no longer live"))?;
        match entry.publication {
            JsonlDirectPublicationState::Registered => {
                entry.publication = JsonlDirectPublicationState::Writing;
                write_frame()?;
                entry.publication = JsonlDirectPublicationState::Published;
                Ok(())
            }
            JsonlDirectPublicationState::Writing | JsonlDirectPublicationState::Published => Err(
                io::Error::other("JSONL direct frame publication already has a witness"),
            ),
        }
    }

    pub(super) fn settle_committed(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<Option<JsonlRequestTombstone>> {
        let entry = self.routes.lock().map_err(lock_error)?.remove(request_id);
        let Some(mut entry) = entry else {
            return Ok(self.admission.tombstone(request_id)?);
        };
        let admission = entry
            .admission
            .take()
            .ok_or_else(|| io::Error::other("JSONL direct admission already consumed"))?;
        self.admission
            .retire(
                admission,
                JsonlRetiredRequestSettlement::DirectInteractionCommitted { response_digest },
            )
            .map(Some)
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
            .ok_or_else(|| io::Error::other("JSONL direct interaction route is no longer live"))?;
        match &entry.state {
            JsonlDirectInteractionState::Routed => {
                entry.state = JsonlDirectInteractionState::CommittedPending {
                    request_id: mutation_request_id,
                    commit_id,
                    response_digest,
                };
                Ok(())
            }
            JsonlDirectInteractionState::CommittedPending {
                request_id: existing_request_id,
                commit_id: existing_commit_id,
                response_digest: existing_response_digest,
            } if existing_request_id == &mutation_request_id
                && existing_commit_id == &commit_id
                && existing_response_digest == &response_digest =>
            {
                Ok(())
            }
            JsonlDirectInteractionState::CommittedPending { .. } => Err(io::Error::other(
                "JSONL direct interaction route has a different committed repair witness",
            )),
        }
    }

    pub(super) fn close_routes(
        &self,
        owner_settlement: super::opaque_permission_router::JsonlOwnerSettlement,
    ) -> io::Result<Vec<JsonlRequestTombstone>> {
        let request_ids = self
            .routes
            .lock()
            .map_err(lock_error)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for request_id in request_ids {
            let entry = {
                let mut routes = self.routes.lock().map_err(lock_error)?;
                if routes.get(&request_id).is_some_and(|entry| {
                    matches!(
                        entry.state,
                        JsonlDirectInteractionState::CommittedPending { .. }
                    )
                }) {
                    None
                } else {
                    routes.remove(&request_id)
                }
            };
            let Some(mut entry) = entry else {
                continue;
            };
            let admission = entry
                .admission
                .take()
                .ok_or_else(|| io::Error::other("JSONL direct admission already consumed"))?;
            tombstones.push(self.admission.retire(
                admission,
                JsonlRetiredRequestSettlement::TransportRetired {
                    owner_settlement: owner_settlement.clone(),
                },
            )?);
        }
        Ok(tombstones)
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
            JsonlRetiredRequestSettlement::DirectInteractionCommitted { response_digest } => {
                response_digest
            }
            JsonlRetiredRequestSettlement::PermissionCommitted { .. }
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

    pub(super) fn settle_committed_pending(&self) -> io::Result<Vec<JsonlRequestTombstone>> {
        let request_ids = self
            .routes
            .lock()
            .map_err(lock_error)?
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.state,
                    JsonlDirectInteractionState::CommittedPending { .. }
                )
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        let mut tombstones = Vec::with_capacity(request_ids.len());
        for request_id in request_ids {
            let response_digest = {
                let routes = self.routes.lock().map_err(lock_error)?;
                let entry = routes
                    .get(&request_id)
                    .ok_or_else(|| io::Error::other("JSONL committed direct route disappeared"))?;
                match entry.state {
                    JsonlDirectInteractionState::CommittedPending {
                        response_digest, ..
                    } => response_digest,
                    JsonlDirectInteractionState::Routed => {
                        return Err(io::Error::other(
                            "JSONL routed direct interaction entered committed settlement",
                        ));
                    }
                }
            };
            if let Some(tombstone) = self.settle_committed(&request_id, response_digest)? {
                tombstones.push(tombstone);
            }
        }
        Ok(tombstones)
    }
}

impl JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute> {
    pub(super) fn settle_unreachable_routes(&self) -> JsonlUnreachableDirectRouteSettlement {
        let routes = self.routes.lock().map_err(lock_error).map(|routes| {
            routes
                .iter()
                .filter(|(_, entry)| matches!(entry.state, JsonlDirectInteractionState::Routed))
                .map(|(request_id, entry)| (request_id.clone(), entry.route.clone()))
                .collect::<Vec<_>>()
        });
        let routes = match routes {
            Ok(routes) => routes,
            Err(error) => {
                return JsonlUnreachableDirectRouteSettlement::unresolved(vec![error.to_string()]);
            }
        };
        let mut unresolved = Vec::new();
        for (request_id, route) in routes {
            let result = (|| -> io::Result<()> {
                let (client, interaction_id, answer, response_digest) = match route {
                    JsonlDirectInteractionRoute::UserInput {
                        client,
                        interaction_id,
                    } => (
                        client,
                        interaction_id,
                        SurfaceClientInteractionAnswer::UserInput {
                            decision: SurfaceUserInputDecision::Cancel,
                        },
                        jsonl_response_digest(&serde_json::json!({ "answer": null }))?,
                    ),
                    JsonlDirectInteractionRoute::McpElicitation {
                        client,
                        interaction_id,
                    } => (
                        client,
                        interaction_id,
                        SurfaceClientInteractionAnswer::McpElicitation {
                            decision: SurfaceMcpElicitationDecision::Decline,
                        },
                        jsonl_response_digest(&serde_json::json!({
                            "accepted": false,
                            "content": null,
                        }))?,
                    ),
                };
                match client.respond_interaction_by_id(
                    SurfaceRequestId::new(),
                    interaction_id,
                    answer,
                ) {
                    Ok(MutationReply::Committed { .. }) => {
                        self.settle_committed(&request_id, response_digest)?;
                    }
                    Ok(MutationReply::Deferred { mutation, .. }) => {
                        self.mark_committed_pending(&request_id, &mutation, response_digest)?;
                    }
                    Ok(MutationReply::Uncommitted { .. }) => {
                        return Err(io::Error::other(
                            "runtime did not commit direct interaction",
                        ));
                    }
                    Err(error) => {
                        return Err(io::Error::other(format!(
                            "runtime rejected direct interaction: {error:?}"
                        )));
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                unresolved.push(format!("{request_id}: {error}"));
            }
        }
        JsonlUnreachableDirectRouteSettlement::unresolved(unresolved)
    }
}

pub(super) struct JsonlUnreachableDirectRouteSettlement {
    unresolved: Vec<String>,
}

impl JsonlUnreachableDirectRouteSettlement {
    fn unresolved(unresolved: Vec<String>) -> Self {
        Self { unresolved }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.unresolved.is_empty()
    }

    pub(super) fn describe(&self) -> String {
        self.unresolved.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::opaque_permission_router::JsonlOwnerSettlement;

    fn admission() -> JsonlConnectionAdmission {
        JsonlConnectionAdmission::new(
            crate::surface::SurfaceConnectionId::try_from_bytes([
                1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 247,
            ])
            .unwrap(),
        )
    }

    #[test]
    fn committed_pending_direct_interaction_is_never_transport_retired() {
        let adapter = JsonlDirectInteractionAdapter::new(admission());
        adapter
            .register(
                "user-input".to_string(),
                JsonlDirectInteractionKind::UserInput,
                "route".to_string(),
            )
            .unwrap();
        adapter
            .mark_committed_pending_witness(
                "user-input",
                crate::surface::SurfaceRequestId::new(),
                crate::surface::SurfaceCommitId::try_from_bytes([
                    1, 159, 161, 19, 220, 41, 112, 211, 145, 70, 17, 0, 120, 212, 79, 248,
                ])
                .unwrap(),
                crate::server::opaque_permission_router::jsonl_response_digest(&"cancel").unwrap(),
            )
            .unwrap();

        assert!(
            adapter
                .close_routes(JsonlOwnerSettlement::InteractionRecoveryRetained)
                .unwrap()
                .is_empty()
        );
        let tombstones = adapter.settle_committed_pending().unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(
            tombstones[0].settlement,
            JsonlRetiredRequestSettlement::DirectInteractionCommitted {
                response_digest: crate::server::opaque_permission_router::jsonl_response_digest(
                    &"cancel"
                )
                .unwrap(),
            }
        );
        assert_eq!(
            adapter
                .committed_replay(
                    "user-input",
                    crate::server::opaque_permission_router::jsonl_response_digest(&"cancel")
                        .unwrap(),
                )
                .unwrap(),
            JsonlCommittedReplay::SameResponse
        );
        assert_eq!(
            adapter
                .committed_replay(
                    "user-input",
                    crate::server::opaque_permission_router::jsonl_response_digest(&"answer")
                        .unwrap(),
                )
                .unwrap(),
            JsonlCommittedReplay::ConflictingResponse
        );
    }

    #[test]
    fn direct_route_publication_holds_the_route_ledger_until_the_frame_is_written() {
        use std::sync::mpsc;

        let adapter = JsonlDirectInteractionAdapter::new(admission());
        adapter
            .register(
                "user-input".to_string(),
                JsonlDirectInteractionKind::UserInput,
                "route".to_string(),
            )
            .unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finish_tx, finish_rx) = mpsc::sync_channel(1);
        let publisher = {
            let adapter = adapter.clone();
            std::thread::spawn(move || {
                adapter.publish("user-input", || {
                    started_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    Ok(())
                })
            })
        };
        started_rx.recv().unwrap();
        assert!(adapter.routes.try_lock().is_err());
        finish_tx.send(()).unwrap();
        publisher.join().unwrap().unwrap();
        assert_eq!(
            adapter
                .published_route("user-input", JsonlDirectInteractionKind::UserInput)
                .unwrap()
                .as_deref(),
            Some("route")
        );
        assert!(
            adapter
                .published_route("user-input", JsonlDirectInteractionKind::McpElicitation)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn direct_registration_cannot_outlive_connection_close_barrier() {
        use std::sync::mpsc;

        let admission = admission();
        let adapter = JsonlDirectInteractionAdapter::new(admission.clone());
        let (barrier_ready_tx, barrier_ready_rx) = mpsc::sync_channel(1);
        let (close_tx, close_rx) = mpsc::sync_channel(1);
        let closer = {
            let admission = admission.clone();
            std::thread::spawn(move || {
                admission
                    .with_route_registration_barrier(|| {
                        barrier_ready_tx.send(()).unwrap();
                        close_rx.recv().unwrap();
                        admission.close_ingress()
                    })
                    .unwrap();
            })
        };
        barrier_ready_rx.recv().unwrap();

        let (registration_started_tx, registration_started_rx) = mpsc::sync_channel(1);
        let registration = std::thread::spawn(move || {
            registration_started_tx.send(()).unwrap();
            adapter.register(
                "late-user-input".to_string(),
                JsonlDirectInteractionKind::UserInput,
                "route".to_string(),
            )
        });
        registration_started_rx.recv().unwrap();
        close_tx.send(()).unwrap();
        closer.join().unwrap();

        assert!(registration.join().unwrap().is_err());
        assert_eq!(admission.counts(), (0, 0, 0));
    }
}
