use std::collections::HashMap;
use std::io;
use std::sync::mpsc::SyncSender;

use sha2::{Digest, Sha256};

use crate::runtime_surface as surface;

pub(crate) struct PreparedInteractionRequest {
    pub(crate) interaction_id: surface::SurfaceInteractionId,
    pub(crate) record: surface::BrokerInteractionRequestRecord,
    pub(crate) route: surface::BrokerInteractionResponseRoute,
    pub(crate) revision: surface::InteractionRevision,
    pub(crate) events: Vec<(surface::SurfaceScope, surface::SurfaceEvent)>,
    pub(crate) unavailable: bool,
}

#[derive(Clone)]
pub(crate) enum ResidentInteractionWaiter {
    ToolApproval {
        approval_id: String,
        waiter: SyncSender<io::Result<orca_core::approval_types::ApprovalResolution>>,
    },
    Permission(SyncSender<io::Result<crate::runtime_permission::RuntimePermissionResponse>>),
    UserInput(SyncSender<io::Result<Option<String>>>),
    McpElicitation(SyncSender<Result<orca_mcp::McpElicitationResponse, String>>),
}

#[derive(Clone)]
pub(crate) struct ResidentSurfaceInteraction {
    pub(crate) record: surface::BrokerInteractionRequestRecord,
    pub(crate) route: surface::BrokerInteractionResponseRoute,
    pub(crate) revision: surface::InteractionRevision,
    pub(crate) waiter: Option<ResidentInteractionWaiter>,
    pub(crate) private_response: Option<ResidentPrivateInteractionResponse>,
    pub(crate) pending_background_route: Option<PendingBackgroundInteractionRoute>,
    pub(crate) winning_receipt: Option<surface::SurfaceInteractionResolutionReceipt>,
    pub(crate) resolution_ack: Option<surface::MutationCommitAck>,
    pub(crate) projected_cursor: Option<surface::SurfaceCursor>,
    pub(crate) cancelled: Option<surface::InteractionCancelReason>,
}

#[derive(Clone)]
pub(crate) struct PendingBackgroundInteractionRoute {
    pub(crate) fence: surface::SurfaceBackgroundFence,
    pub(crate) batch: surface::SurfaceCommitBatch,
    pub(crate) next_revision: surface::InteractionRevision,
    pub(crate) private_route: surface::BrokerInteractionResponseRoute,
    pub(crate) retry_at: tokio::time::Instant,
}

#[derive(Clone)]
pub(crate) struct ResidentPrivateInteractionResponse {
    pub(crate) record: surface::BrokerInteractionResponseRecord,
    pub(crate) answer: surface::SurfaceClientInteractionAnswer,
    pub(crate) pending_batch: Option<surface::SurfaceCommitBatch>,
    pub(crate) retry_at: Option<tokio::time::Instant>,
}

pub(crate) struct InteractionController<
    ToolRecoveryOwner = (),
    PermissionRecoveryOwner = (),
    ContinuationRecoveryOwner = (),
    Detach = (),
    CapabilityLoss = (),
> {
    interactions: HashMap<surface::SurfaceInteractionId, ResidentSurfaceInteraction>,
    pub(crate) cold_recovery_owners: HashMap<surface::SurfaceInteractionId, ToolRecoveryOwner>,
    pub(crate) cold_recovery_permission_owners:
        HashMap<surface::SurfaceInteractionId, PermissionRecoveryOwner>,
    pub(crate) continuation_turn_owners:
        HashMap<surface::SurfaceInteractionId, ContinuationRecoveryOwner>,
    pub(crate) operation_origin_attachments:
        HashMap<surface::SurfaceOperationId, surface::SurfaceAttachmentId>,
    pub(crate) pending_detaches: HashMap<surface::SurfaceAttachmentId, Detach>,
    pub(crate) pending_capability_losses: HashMap<surface::SurfaceAttachmentId, CapabilityLoss>,
}

impl<ToolRecoveryOwner, PermissionRecoveryOwner, ContinuationRecoveryOwner, Detach, CapabilityLoss>
    InteractionController<
        ToolRecoveryOwner,
        PermissionRecoveryOwner,
        ContinuationRecoveryOwner,
        Detach,
        CapabilityLoss,
    >
{
    pub(crate) fn new(
        interactions: HashMap<surface::SurfaceInteractionId, ResidentSurfaceInteraction>,
        cold_recovery_owners: HashMap<surface::SurfaceInteractionId, ToolRecoveryOwner>,
        cold_recovery_permission_owners: HashMap<
            surface::SurfaceInteractionId,
            PermissionRecoveryOwner,
        >,
        continuation_turn_owners: HashMap<surface::SurfaceInteractionId, ContinuationRecoveryOwner>,
    ) -> Self {
        Self {
            interactions,
            cold_recovery_owners,
            cold_recovery_permission_owners,
            continuation_turn_owners,
            operation_origin_attachments: HashMap::new(),
            pending_detaches: HashMap::new(),
            pending_capability_losses: HashMap::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        interaction_id: surface::SurfaceInteractionId,
        interaction: ResidentSurfaceInteraction,
    ) -> Option<ResidentSurfaceInteraction> {
        self.interactions.insert(interaction_id, interaction)
    }

    pub(crate) fn get(
        &self,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Option<&ResidentSurfaceInteraction> {
        self.interactions.get(interaction_id)
    }

    pub(crate) fn get_mut(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Option<&mut ResidentSurfaceInteraction> {
        self.interactions.get_mut(interaction_id)
    }

    pub(crate) fn contains_key(&self, interaction_id: &surface::SurfaceInteractionId) -> bool {
        self.interactions.contains_key(interaction_id)
    }

    pub(crate) fn remove(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Option<ResidentSurfaceInteraction> {
        self.interactions.remove(interaction_id)
    }

    pub(crate) fn iter(
        &self,
    ) -> impl Iterator<Item = (&surface::SurfaceInteractionId, &ResidentSurfaceInteraction)> {
        self.interactions.iter()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &ResidentSurfaceInteraction> {
        self.interactions.values()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.interactions.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.interactions.is_empty()
    }
}

pub(crate) type ResidentInteractionController<
    ToolRecoveryOwner = (),
    PermissionRecoveryOwner = (),
    ContinuationRecoveryOwner = (),
    Detach = (),
    CapabilityLoss = (),
> = InteractionController<
    ToolRecoveryOwner,
    PermissionRecoveryOwner,
    ContinuationRecoveryOwner,
    Detach,
    CapabilityLoss,
>;

pub(crate) fn prepare_interaction_request(
    fence: surface::SurfaceOperationFence,
    interaction_id: surface::SurfaceInteractionId,
    kind: surface::SurfaceInteractionKind,
    request: surface::SurfaceInteractionRequest,
    recovery_disposition: surface::InteractionUnavailableDisposition,
    attachment_id: Option<surface::SurfaceAttachmentId>,
) -> PreparedInteractionRequest {
    let unavailable = attachment_id.is_none();
    let revision = surface::InteractionRevision::try_new(1).expect("one is valid");
    let route_epoch = surface::ResponseRouteEpoch::try_new(1).expect("one is valid");
    let record = surface::BrokerInteractionRequestRecord {
        thread_id: fence.thread_id.clone(),
        interaction_id: interaction_id.clone(),
        fence: fence.clone(),
        kind,
        request: request.clone(),
        response_token: surface::SurfaceResponseToken::new(random_token_bytes()),
        answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
        recovery_disposition,
    };
    let route = match attachment_id.as_ref() {
        Some(attachment_id) => surface::BrokerInteractionResponseRoute::Exclusive {
            epoch: route_epoch,
            attachment_id: attachment_id.clone(),
            grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
        },
        None => surface::BrokerInteractionResponseRoute::Unassigned { epoch: route_epoch },
    };
    let public_route = match attachment_id {
        Some(attachment_id) => surface::SurfaceInteractionRoute::Exclusive {
            epoch: route_epoch,
            attachment_id,
        },
        None => surface::SurfaceInteractionRoute::Unassigned { epoch: route_epoch },
    };
    let view = surface::SurfaceInteractionView {
        interaction_id: interaction_id.clone(),
        revision,
        fence: fence.clone(),
        kind,
        request,
        route: public_route,
        lifecycle: surface::SurfaceInteractionLifecycle::Requested,
        recovery_disposition: record.recovery_disposition.clone(),
    };
    let mut events = vec![(
        surface::SurfaceScope::Generation {
            fence: fence.clone(),
        },
        surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
            interaction: view,
        }),
    )];
    if unavailable {
        events.push((
            surface::SurfaceScope::Generation {
                fence: fence.clone(),
            },
            surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                interaction_id: interaction_id.clone(),
                expected_revision: revision,
                next_revision: surface::InteractionRevision::try_new(revision.get() + 1)
                    .expect("interaction revision did not exhaust"),
                reason: surface::InteractionCancelReason::CapabilityUnavailable,
            }),
        ));
    }
    PreparedInteractionRequest {
        interaction_id,
        record,
        route,
        revision,
        events,
        unavailable,
    }
}

fn random_token_bytes() -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes
}

pub(crate) fn keyed_interaction_response_digest(
    token: &surface::SurfaceResponseToken,
    answer: &surface::SurfaceClientInteractionAnswer,
) -> surface::OpaqueToken {
    let mut hasher = Sha256::new();
    hasher.update(token.key_bytes());
    hasher.update(serde_json::to_vec(answer).expect("interaction answer serializes"));
    surface::OpaqueToken::new(hasher.finalize().into())
}

pub(crate) fn interaction_route_attachments(
    route: &surface::BrokerInteractionResponseRoute,
) -> Vec<surface::SurfaceAttachmentId> {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => Vec::new(),
        surface::BrokerInteractionResponseRoute::Exclusive { attachment_id, .. } => {
            vec![attachment_id.clone()]
        }
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { grants, .. } => grants
            .as_slice()
            .iter()
            .map(|(attachment_id, _)| attachment_id.clone())
            .collect(),
    }
}

pub(crate) fn interaction_route_admits(
    route: &surface::BrokerInteractionResponseRoute,
    attachment_id: &surface::SurfaceAttachmentId,
) -> bool {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => false,
        surface::BrokerInteractionResponseRoute::Exclusive {
            attachment_id: expected,
            ..
        } => expected == attachment_id,
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { grants, .. } => grants
            .as_slice()
            .iter()
            .any(|(expected, _)| expected == attachment_id),
    }
}

pub(crate) fn interaction_route_admits_exact(
    route: &surface::BrokerInteractionResponseRoute,
    attachment_id: &surface::SurfaceAttachmentId,
    route_epoch: surface::ResponseRouteEpoch,
    grant_token: &surface::SurfaceResponseGrantToken,
) -> bool {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => false,
        surface::BrokerInteractionResponseRoute::Exclusive {
            epoch,
            attachment_id: expected_attachment,
            grant_token: expected_grant,
        } => {
            *epoch == route_epoch
                && expected_attachment == attachment_id
                && expected_grant == grant_token
        }
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins {
            epoch, grants, ..
        } => {
            *epoch == route_epoch
                && grants
                    .as_slice()
                    .iter()
                    .any(|(expected_attachment, expected_grant)| {
                        expected_attachment == attachment_id && expected_grant == grant_token
                    })
        }
    }
}

pub(crate) fn interaction_route_epoch(
    route: &surface::BrokerInteractionResponseRoute,
) -> surface::ResponseRouteEpoch {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { epoch }
        | surface::BrokerInteractionResponseRoute::Exclusive { epoch, .. }
        | surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { epoch, .. } => *epoch,
    }
}

pub(crate) fn exact_interaction_selectors(
    interaction: &ResidentSurfaceInteraction,
) -> Vec<(surface::SurfaceAttachmentId, surface::InteractionSelector)> {
    let grants = match &interaction.route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => Vec::new(),
        surface::BrokerInteractionResponseRoute::Exclusive {
            epoch,
            attachment_id,
            grant_token,
        } => vec![(attachment_id.clone(), *epoch, grant_token.clone())],
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins {
            epoch, grants, ..
        } => grants
            .as_slice()
            .iter()
            .map(|(attachment_id, grant_token)| {
                (attachment_id.clone(), *epoch, grant_token.clone())
            })
            .collect(),
    };
    grants
        .into_iter()
        .map(
            |(attachment_id, response_route_epoch, response_grant_token)| {
                (
                    attachment_id,
                    surface::InteractionSelector::Exact {
                        interaction_id: interaction.record.interaction_id.clone(),
                        expected_revision: interaction.revision,
                        kind: interaction.record.kind,
                        response_token: interaction.record.response_token.clone(),
                        response_route_epoch,
                        response_grant_token,
                        operation_fence: interaction.record.fence.clone(),
                    },
                )
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ResidentSurfaceInteraction, exact_interaction_selectors, interaction_route_admits_exact,
        prepare_interaction_request,
    };
    use crate::runtime_surface as surface;

    fn test_fence() -> surface::SurfaceOperationFence {
        surface::SurfaceOperationFence {
            thread_id: surface::SurfaceThreadId::try_from_bytes(*uuid::Uuid::new_v4().as_bytes())
                .unwrap(),
            thread_owner_epoch: surface::ThreadOwnerEpoch::new(1),
            operation_id: surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            generation_id: surface::SurfaceGenerationId::new(0),
        }
    }

    fn test_interaction_id() -> surface::SurfaceInteractionId {
        surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap()
    }

    fn test_request() -> surface::SurfaceInteractionRequest {
        surface::SurfaceInteractionRequest::UserInput {
            question: surface::NonEmptyText::try_new("continue?").unwrap(),
            suggestions: vec![surface::DisplayText::new("yes")],
        }
    }

    #[test]
    fn prepared_request_with_attachment_has_exact_route_and_requested_event_bits_spec_ut() {
        let fence = test_fence();
        let interaction_id = test_interaction_id();
        let attachment_id =
            surface::SurfaceAttachmentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let prepared = prepare_interaction_request(
            fence.clone(),
            interaction_id.clone(),
            surface::SurfaceInteractionKind::UserInput,
            test_request(),
            surface::InteractionUnavailableDisposition::FailOperation,
            Some(attachment_id.clone()),
        );

        assert!(!prepared.unavailable);
        assert_eq!(prepared.revision.get(), 1);
        assert_eq!(prepared.events.len(), 1);
        let (route_epoch, grant_token) = match &prepared.route {
            surface::BrokerInteractionResponseRoute::Exclusive {
                epoch,
                attachment_id: routed_attachment,
                grant_token,
            } => {
                assert_eq!(routed_attachment, &attachment_id);
                (*epoch, grant_token.clone())
            }
            surface::BrokerInteractionResponseRoute::Unassigned { .. }
            | surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { .. } => {
                panic!("prepared attached request must use an exclusive route")
            }
        };
        match &prepared.events[0] {
            (
                surface::SurfaceScope::Generation { fence: event_fence },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                    interaction,
                }),
            ) => {
                assert_eq!(event_fence, &fence);
                assert_eq!(interaction.interaction_id, interaction_id);
                assert_eq!(interaction.revision, prepared.revision);
                assert!(matches!(
                    &interaction.route,
                    surface::SurfaceInteractionRoute::Exclusive {
                        epoch,
                        attachment_id: event_attachment,
                    } if *epoch == route_epoch && event_attachment == &attachment_id
                ));
                assert!(matches!(
                    interaction.lifecycle,
                    surface::SurfaceInteractionLifecycle::Requested
                ));
            }
            _ => panic!("prepared attached request must emit one requested event"),
        }

        let resident = ResidentSurfaceInteraction {
            record: prepared.record.clone(),
            route: prepared.route.clone(),
            revision: prepared.revision,
            waiter: None,
            private_response: None,
            pending_background_route: None,
            winning_receipt: None,
            resolution_ack: None,
            projected_cursor: None,
            cancelled: None,
        };
        let selectors = exact_interaction_selectors(&resident);
        assert_eq!(selectors.len(), 1);
        let (selector_attachment, selector) = &selectors[0];
        assert_eq!(selector_attachment, &attachment_id);
        assert!(matches!(
            selector,
            surface::InteractionSelector::Exact {
                interaction_id: selector_interaction_id,
                expected_revision,
                kind: surface::SurfaceInteractionKind::UserInput,
                response_token,
                response_route_epoch,
                response_grant_token,
                operation_fence,
            } if selector_interaction_id == &interaction_id
                && *expected_revision == prepared.revision
                && response_token == &prepared.record.response_token
                && *response_route_epoch == route_epoch
                && response_grant_token == &grant_token
                && operation_fence == &fence
        ));
    }

    #[test]
    fn prepared_request_without_attachment_cancels_after_requested_event_bits_spec_ut() {
        let fence = test_fence();
        let interaction_id = test_interaction_id();
        let prepared = prepare_interaction_request(
            fence.clone(),
            interaction_id.clone(),
            surface::SurfaceInteractionKind::UserInput,
            test_request(),
            surface::InteractionUnavailableDisposition::FailOperation,
            None,
        );

        assert!(prepared.unavailable);
        assert_eq!(prepared.revision.get(), 1);
        assert!(matches!(
            prepared.route,
            surface::BrokerInteractionResponseRoute::Unassigned { epoch }
                if epoch.get() == 1
        ));
        assert_eq!(prepared.events.len(), 2);
        assert!(matches!(
            &prepared.events[0],
            (
                surface::SurfaceScope::Generation { fence: event_fence },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                    interaction,
                }),
            ) if event_fence == &fence
                && interaction.interaction_id == interaction_id
                && matches!(
                    interaction.route,
                    surface::SurfaceInteractionRoute::Unassigned { epoch } if epoch.get() == 1
                )
        ));
        assert!(matches!(
            &prepared.events[1],
            (
                surface::SurfaceScope::Generation { fence: event_fence },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: cancelled_interaction_id,
                    expected_revision,
                    next_revision,
                    reason: surface::InteractionCancelReason::CapabilityUnavailable,
                }),
            ) if event_fence == &fence
                && cancelled_interaction_id == &interaction_id
                && expected_revision.get() == 1
                && next_revision.get() == 2
        ));
    }

    #[test]
    fn exact_route_requires_matching_epoch_attachment_and_grant_bits_spec_ut() {
        let attachment_id =
            surface::SurfaceAttachmentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let route_epoch = surface::ResponseRouteEpoch::try_new(3).unwrap();
        let grant = surface::SurfaceResponseGrantToken::new([7; 32]);
        let route = surface::BrokerInteractionResponseRoute::Exclusive {
            epoch: route_epoch,
            attachment_id: attachment_id.clone(),
            grant_token: grant.clone(),
        };
        assert!(interaction_route_admits_exact(
            &route,
            &attachment_id,
            route_epoch,
            &grant,
        ));
        assert!(!interaction_route_admits_exact(
            &route,
            &attachment_id,
            surface::ResponseRouteEpoch::try_new(4).unwrap(),
            &grant,
        ));
    }
}
