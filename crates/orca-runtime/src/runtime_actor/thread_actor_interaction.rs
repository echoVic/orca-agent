// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

const DETACHED_PERMISSION_DRAIN_LIMIT: usize = 32;

enum DetachedPermissionAdmissionError {
    Deferred(String),
    Denied(String),
}

impl ThreadActor {
    /// Consume durable permission requests emitted by detached child workers.
    ///
    /// The mailbox is intentionally drained by the actor (rather than by a
    /// UI adapter): this keeps owner/fence validation and the interaction
    /// commit in one serialization domain. A request is only removed by the
    /// child after the actor has persisted a response, so retries here are
    /// naturally idempotent.
    pub(super) fn drain_detached_permission_requests(&mut self, active: Option<&ActiveOperation>) {
        // A sessionless actor can receive the periodic tick before a typed
        // surface is admitted.  There is no resident waiter to retry in that
        // state, and touching the fail-fast surface slot would panic.
        if self.resident_surface.0.is_none() {
            return;
        }
        // A surface resolution is committed before the detached mailbox is
        // updated. Retry any response whose first persistence attempt failed
        // before admitting new requests; the resident waiter is deliberately
        // retained until this succeeds.
        self.retry_detached_permission_resolutions();
        let registry = active
            .map(|active| active.task_registry.clone())
            .or_else(|| {
                self.state
                    .as_ref()
                    .map(|state| state.thread.session().task_registry().clone())
            });
        let Some(registry) = registry else {
            return;
        };
        if let Err(error) = registry.prune_detached_permission_requests() {
            eprintln!("orca: detached permission mailbox compaction failed: {error}");
        }
        let requests = match registry.detached_permission_requests() {
            Ok(requests) => requests,
            Err(error) => {
                eprintln!("orca: detached permission mailbox scan failed: {error}");
                return;
            }
        };
        for request in requests
            .into_iter()
            .filter(DetachedPermissionRequest::is_pending)
            .take(DETACHED_PERMISSION_DRAIN_LIMIT)
        {
            match self.admit_detached_permission_request(&registry, &request) {
                Ok(()) => {}
                Err(DetachedPermissionAdmissionError::Deferred(_reason)) => {
                    // A child can publish its mailbox entry just before its
                    // Started/activity frame reaches the surface. Keep the
                    // entry pending and let the next actor tick retry it.
                }
                Err(DetachedPermissionAdmissionError::Denied(reason)) => {
                    // Invalid owner material is terminal for this request.
                    // Persisting a deny wakes the child and prevents an
                    // unbounded mailbox entry from blocking thread close.
                    eprintln!(
                        "orca: denying detached permission request {}: {reason}",
                        request.key
                    );
                    if let Err(error) = resolve_detached_permission_deny(
                        &registry,
                        &request.key,
                        &request.request_digest,
                        &reason,
                    ) {
                        eprintln!(
                            "orca: detached permission denial remains pending for {}: {error}",
                            request.key
                        );
                    }
                }
            }
        }
    }

    /// Finish mailbox settlement for a detached interaction whose public
    /// surface resolution already committed. The durable receipt contains the
    /// exact permission profile, so this remains recoverable without retaining
    /// the client's private response or issuing a second prompt.
    fn retry_detached_permission_resolutions(&mut self) {
        let candidates = self
            .resident_surface
            .interactions
            .iter()
            .filter_map(|(interaction_id, interaction)| {
                let ResidentInteractionWaiter::DetachedPermission {
                    registry,
                    key,
                    request_digest,
                } = interaction.waiter.as_ref()?
                else {
                    return None;
                };
                let receipt = interaction.winning_receipt.as_ref()?;
                let response = detached_permission_response_from_receipt(receipt)?;
                Some((
                    interaction_id.clone(),
                    registry.clone(),
                    key.clone(),
                    request_digest.clone(),
                    response,
                ))
            })
            .collect::<Vec<_>>();
        for (interaction_id, registry, key, request_digest, response) in candidates {
            let expected_response = response.clone();
            match registry.resolve_detached_permission_request(&key, &request_digest, response) {
                Ok(()) => {
                    if let Some(interaction) =
                        self.resident_surface.interactions.get_mut(&interaction_id)
                        && matches!(
                            interaction.waiter.as_ref(),
                            Some(ResidentInteractionWaiter::DetachedPermission {
                                key: current_key,
                                request_digest: current_digest,
                                ..
                            }) if current_key == &key && current_digest == &request_digest
                        )
                    {
                        interaction.waiter = None;
                        interaction.private_response = None;
                    }
                }
                Err(error) => {
                    // The child may have acknowledged the response, or a
                    // fail-closed cancellation may have won the race, between
                    // our scan and this retry. A missing record is already
                    // acknowledged; an existing record only settles this
                    // waiter when it contains the exact response we tried to
                    // persist. A different response is a conflict and must
                    // remain visible for diagnosis/recovery.
                    let terminal = match registry.detached_permission_request(&key) {
                        Ok(None) => true,
                        Ok(Some(record)) => record.response.as_ref() == Some(&expected_response),
                        Err(_) => false,
                    };
                    if terminal {
                        if let Some(interaction) =
                            self.resident_surface.interactions.get_mut(&interaction_id)
                            && matches!(
                                interaction.waiter.as_ref(),
                                Some(ResidentInteractionWaiter::DetachedPermission {
                                    key: current_key,
                                    request_digest: current_digest,
                                    ..
                                }) if current_key == &key && current_digest == &request_digest
                            )
                        {
                            interaction.waiter = None;
                            interaction.private_response = None;
                        }
                    } else {
                        eprintln!(
                            "orca: detached permission response settlement deferred for {key}: {error}"
                        );
                    }
                }
            }
        }
    }

    fn admit_detached_permission_request(
        &mut self,
        registry: &TaskRegistry,
        mailbox: &DetachedPermissionRequest,
    ) -> Result<(), DetachedPermissionAdmissionError> {
        let crate::runtime_permission::RuntimePermissionContext::Child {
            task_id,
            task_revision,
            agent_id,
            agent_revision,
            activity_id: _activity_id,
            turn_id,
            tool_call_id,
            origin,
        } = &mailbox.request.context
        else {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission request is not child-owned".to_string(),
            ));
        };
        let binding = match registry.detached_subagent_binding(task_id.as_str()) {
            Ok(Some(binding)) => binding,
            Ok(None) => {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached subagent owner binding is missing or stale".to_string(),
                ));
            }
            Err(error) => {
                return Err(DetachedPermissionAdmissionError::Deferred(format!(
                    "owner binding lookup failed: {error}"
                )));
            }
        };
        let expected_key = format!(
            "{}:{}:{}",
            binding.task_id,
            binding.attempt_id.as_str(),
            mailbox.request.id
        );
        if mailbox.request.id.is_empty()
            || mailbox.key != expected_key
            || mailbox.request.id != tool_call_id.as_str()
            || *origin != surface::SurfacePermissionOrigin::ChildAgent
            || *task_revision != binding.task_revision
            || agent_id.as_str() != binding.subagent_id
            || mailbox.attempt_id != binding.attempt_id
            || mailbox.authority_digest != binding.authority_digest
            || mailbox.permission_response_public_key != binding.permission_response_public_key
        {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission request owner tuple is invalid".to_string(),
            ));
        }
        let expected_interaction_id =
            match detached_permission_interaction_id(&binding, &mailbox.request.id) {
                Ok(interaction_id) => interaction_id,
                Err(error) => {
                    return Err(DetachedPermissionAdmissionError::Denied(format!(
                        "detached permission interaction identity is invalid: {error}"
                    )));
                }
            };
        if mailbox.interaction_id != expected_interaction_id {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission interaction identity is not actor-derived".to_string(),
            ));
        }
        let Some(parent_fence) = binding.parent_fence.clone() else {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission binding has no parent operation fence".to_string(),
            ));
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        if snapshot.thread.thread_id != parent_fence.thread_id {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission parent fence belongs to another thread".to_string(),
            ));
        }
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == *task_id)
            .ok_or_else(|| {
                DetachedPermissionAdmissionError::Deferred(
                    "detached permission task projection has not arrived".to_string(),
                )
            })?;
        let subagent = snapshot
            .subagents
            .iter()
            .find(|subagent| subagent.subagent_id == *agent_id)
            .ok_or_else(|| {
                DetachedPermissionAdmissionError::Deferred(
                    "detached permission subagent projection has not arrived".to_string(),
                )
            })?;
        if task.task_type != surface::SurfaceTaskType::Subagent
            || task.subagent_id.as_ref() != Some(agent_id)
            || task.parent_task_id.as_ref().map(|parent| parent.as_str())
                != binding.parent_task_id.as_deref()
            || subagent.task_id != *task_id
            || subagent.source.attempt_id.as_str() != binding.attempt_id.as_str()
            || subagent.source.turn_id != *turn_id
            || subagent.source.attempt_id.as_str() != mailbox.attempt_id.as_str()
        {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission task/subagent identity is invalid".to_string(),
            ));
        }
        let surface::SurfaceSubagentOwner::DetachedTask { owner } = &subagent.owner else {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission subagent is generation-owned".to_string(),
            ));
        };
        if owner.task_id != *task_id
            || owner.task_revision != binding.task_revision
            || owner.attempt_id.as_str() != binding.attempt_id.as_str()
            || owner.authority_digest != binding.authority_digest
            || subagent.revision < *agent_revision
            || agent_revision.get() == 0
        {
            if subagent.revision < *agent_revision {
                return Err(DetachedPermissionAdmissionError::Deferred(
                    "detached permission activity projection has not arrived".to_string(),
                ));
            }
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission subagent owner epoch is stale".to_string(),
            ));
        }
        if task.revision < *task_revision {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission task revision is behind its admission floor".to_string(),
            ));
        }
        let tool = Self::surface_child_permission_tool(&mailbox.request).map_err(|error| {
            DetachedPermissionAdmissionError::Denied(format!(
                "detached permission tool identity is invalid: {error}"
            ))
        })?;
        let permissions = surface_permission_profile_from_runtime(
            mailbox.request.permissions.clone(),
        )
        .map_err(|error| {
            DetachedPermissionAdmissionError::Denied(format!(
                "detached permission profile is invalid: {error}"
            ))
        })?;
        let authority =
            Self::surface_authority_for_tool(&snapshot, &parent_fence, &tool).map_err(|error| {
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
                ) {
                    DetachedPermissionAdmissionError::Deferred(format!(
                        "detached permission authority is not materialized: {error}"
                    ))
                } else {
                    DetachedPermissionAdmissionError::Denied(format!(
                        "detached permission authority is invalid: {error}"
                    ))
                }
            })?;

        // A surface resolution is committed before the actor writes the
        // process-independent mailbox response. If the actor crashes in that
        // window, recover the exact persisted answer from the receipt instead
        // of creating a second prompt or converting an allow into a deny.
        if let Some(view) = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == mailbox.interaction_id)
            && let surface::SurfaceInteractionLifecycle::Resolved { receipt } = &view.lifecycle
        {
            if view.fence != parent_fence
                || receipt.kind != surface::SurfaceInteractionKind::PermissionRequest
                || !detached_surface_request_matches_mailbox(
                    &view.request,
                    &mailbox.request,
                    &binding,
                    &permissions,
                    &authority,
                    task.revision,
                )
            {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission resolution owner or request changed".to_string(),
                ));
            }
            let response = detached_permission_response_from_receipt(receipt).ok_or_else(|| {
                DetachedPermissionAdmissionError::Denied(
                    "detached permission resolution receipt is incomplete".to_string(),
                )
            })?;
            if response.permissions
                != mailbox
                    .request
                    .permissions
                    .clone()
                    .normalize_file_system_entries()
            {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission resolution profile does not match mailbox".to_string(),
                ));
            }
            registry
                .resolve_detached_permission_request(
                    &mailbox.key,
                    &mailbox.request_digest,
                    response,
                )
                .map_err(|error| {
                    DetachedPermissionAdmissionError::Deferred(format!(
                        "detached permission recovery response persistence deferred: {error}"
                    ))
                })?;
            return Ok(());
        }

        // Cancellation/expiry is also a terminal mailbox outcome. The
        // interaction may have been removed from the resident map during the
        // capability-loss path, so recover the durable lifecycle and persist a
        // deny instead of admitting a fresh prompt after restart.
        if let Some(view) = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == mailbox.interaction_id)
            && matches!(
                view.lifecycle,
                surface::SurfaceInteractionLifecycle::Cancelled { .. }
                    | surface::SurfaceInteractionLifecycle::Expired { .. }
            )
        {
            if view.fence != parent_fence
                || view.kind != surface::SurfaceInteractionKind::PermissionRequest
                || !detached_surface_request_matches_mailbox(
                    &view.request,
                    &mailbox.request,
                    &binding,
                    &permissions,
                    &authority,
                    task.revision,
                )
            {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission terminal interaction owner or request changed".to_string(),
                ));
            }
            resolve_detached_permission_deny(
                registry,
                &mailbox.key,
                &mailbox.request_digest,
                "surface interaction was cancelled or expired",
            )
            .map_err(DetachedPermissionAdmissionError::Deferred)?;
            return Ok(());
        }

        // A request that is already parked in ApprovalRequired state is the
        // normal restart/race case. Reattach the durable mailbox waiter to
        // that exact interaction instead of emitting a second Requested fact.
        if task.status == surface::SurfaceTaskStatus::ApprovalRequired {
            if subagent.status != surface::SurfaceSubagentStatus::Running {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission subagent is no longer running".to_string(),
                ));
            }
            if task.pending_interaction_id.as_ref() != Some(&mailbox.interaction_id) {
                return Err(DetachedPermissionAdmissionError::Deferred(
                    "another child permission interaction is currently pending".to_string(),
                ));
            }
            let Some(view) = snapshot
                .interactions
                .iter()
                .find(|interaction| interaction.interaction_id == mailbox.interaction_id)
            else {
                return Err(DetachedPermissionAdmissionError::Deferred(
                    "detached permission interaction projection has not arrived".to_string(),
                ));
            };
            if !matches!(
                view.lifecycle,
                surface::SurfaceInteractionLifecycle::Requested
            ) || view.fence != parent_fence
                || !detached_surface_request_matches_mailbox(
                    &view.request,
                    &mailbox.request,
                    &binding,
                    &permissions,
                    &authority,
                    task.revision,
                )
            {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission interaction owner or request changed".to_string(),
                ));
            }
            return self.ensure_detached_permission_resident(registry, mailbox, view);
        }
        if task.status != surface::SurfaceTaskStatus::Running {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission task is no longer running".to_string(),
            ));
        }
        if task.pending_interaction_id.is_some() {
            return Err(DetachedPermissionAdmissionError::Deferred(
                "detached permission task already has a pending interaction".to_string(),
            ));
        }
        if subagent.status != surface::SurfaceSubagentStatus::Running {
            return Err(DetachedPermissionAdmissionError::Denied(
                "detached permission subagent is no longer running".to_string(),
            ));
        }
        let context = Self::surface_permission_context_for_tool(
            &snapshot,
            &parent_fence,
            &tool,
            &mailbox.request.context,
        )
        .map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
            ) {
                DetachedPermissionAdmissionError::Deferred(error.to_string())
            } else {
                DetachedPermissionAdmissionError::Denied(error.to_string())
            }
        })?;
        let interaction_request = surface::SurfaceInteractionRequest::PermissionRequest {
            tool_call_id: tool.tool_call_id.clone(),
            context,
            reason: mailbox
                .request
                .reason
                .clone()
                .map(surface::DisplayText::new),
            permissions,
            authority,
        };
        if let Some(view) = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == mailbox.interaction_id)
        {
            if !matches!(
                view.lifecycle,
                surface::SurfaceInteractionLifecycle::Requested
            ) {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission interaction is already terminal".to_string(),
                ));
            }
            if view.fence != parent_fence || view.request != interaction_request {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "detached permission interaction request conflicts with mailbox".to_string(),
                ));
            }
            return self.ensure_detached_permission_resident(registry, mailbox, view);
        }
        if let Some(existing) = self
            .resident_surface
            .interactions
            .get(&mailbox.interaction_id)
        {
            if existing.record.fence != parent_fence
                || existing.record.request != interaction_request
            {
                return Err(DetachedPermissionAdmissionError::Denied(
                    "resident detached permission interaction conflicts with mailbox".to_string(),
                ));
            }
            return self.attach_detached_permission_waiter(registry, mailbox);
        }
        let preferred_attachment = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&parent_fence.operation_id)
            .cloned();
        let attachment_id = self.resident_surface.hub.select_interaction_attachment_for(
            surface::SurfaceInteractionKind::PermissionRequest,
            preferred_attachment.as_ref(),
        );
        let prepared = prepare_detached_interaction_request(
            parent_fence.clone(),
            mailbox.interaction_id.clone(),
            surface::SurfaceInteractionKind::PermissionRequest,
            interaction_request,
            surface::InteractionUnavailableDisposition::FailOperation,
            attachment_id,
        );
        let next_task_revision = surface::TaskRevision::try_new(
            task.revision.get().checked_add(1).ok_or_else(|| {
                DetachedPermissionAdmissionError::Denied(
                    "detached permission task revision exhausted".to_string(),
                )
            })?,
        )
        .map_err(|_| {
            DetachedPermissionAdmissionError::Denied(
                "detached permission task revision is invalid".to_string(),
            )
        })?;
        let mut events = prepared.events;
        events.push((
            surface::SurfaceScope::Thread,
            surface::SurfaceEvent::Task(surface::TaskPatch::InteractionChanged {
                task_id: task.task_id.clone(),
                expected_revision: task.revision,
                next_revision: next_task_revision,
                status: surface::SurfaceTaskStatus::ApprovalRequired,
                pending_interaction_id: Some(mailbox.interaction_id.clone()),
            }),
        ));
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_actor_generation_permission_request_batch(parent_fence, &batch)
            .map_err(|error| {
                DetachedPermissionAdmissionError::Deferred(format!(
                    "detached permission request commit deferred: {error:?}"
                ))
            })?;
        self.resident_surface.interactions.insert(
            mailbox.interaction_id.clone(),
            ResidentSurfaceInteraction {
                record: prepared.record,
                route: prepared.route,
                revision: prepared.revision,
                waiter: Some(ResidentInteractionWaiter::DetachedPermission {
                    registry: registry.clone(),
                    key: mailbox.key.clone(),
                    request_digest: mailbox.request_digest.clone(),
                }),
                private_response: None,
                pending_background_route: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    fn attach_detached_permission_waiter(
        &mut self,
        registry: &TaskRegistry,
        mailbox: &DetachedPermissionRequest,
    ) -> Result<(), DetachedPermissionAdmissionError> {
        let Some(interaction) = self
            .resident_surface
            .interactions
            .get_mut(&mailbox.interaction_id)
        else {
            return Err(DetachedPermissionAdmissionError::Deferred(
                "resident detached permission interaction is not available yet".to_string(),
            ));
        };
        if interaction.winning_receipt.is_some() || interaction.cancelled.is_some() {
            return Err(DetachedPermissionAdmissionError::Denied(
                "resident detached permission interaction is terminal".to_string(),
            ));
        }
        match &interaction.waiter {
            Some(ResidentInteractionWaiter::DetachedPermission {
                key,
                request_digest,
                ..
            }) if key == &mailbox.key && request_digest == &mailbox.request_digest => Ok(()),
            Some(_) => Err(DetachedPermissionAdmissionError::Denied(
                "resident interaction is owned by another waiter".to_string(),
            )),
            None => {
                interaction.waiter = Some(ResidentInteractionWaiter::DetachedPermission {
                    registry: registry.clone(),
                    key: mailbox.key.clone(),
                    request_digest: mailbox.request_digest.clone(),
                });
                Ok(())
            }
        }
    }

    fn ensure_detached_permission_resident(
        &mut self,
        registry: &TaskRegistry,
        mailbox: &DetachedPermissionRequest,
        view: &surface::SurfaceInteractionView,
    ) -> Result<(), DetachedPermissionAdmissionError> {
        if self
            .resident_surface
            .interactions
            .contains_key(&mailbox.interaction_id)
        {
            return self.attach_detached_permission_waiter(registry, mailbox);
        }
        let mut revision = view.revision;
        let route = match &view.route {
            surface::SurfaceInteractionRoute::Unassigned { epoch } => {
                surface::BrokerInteractionResponseRoute::Unassigned { epoch: *epoch }
            }
            surface::SurfaceInteractionRoute::Exclusive { epoch, .. }
            | surface::SurfaceInteractionRoute::SharedFirstCommitWins { epoch, .. } => {
                let next_revision = surface::InteractionRevision::try_new(
                    revision.get().checked_add(1).ok_or_else(|| {
                        DetachedPermissionAdmissionError::Denied(
                            "detached permission interaction revision exhausted".to_string(),
                        )
                    })?,
                )
                .map_err(|_| {
                    DetachedPermissionAdmissionError::Denied(
                        "detached permission interaction revision is invalid".to_string(),
                    )
                })?;
                let next_epoch = surface::ResponseRouteEpoch::try_new(
                    epoch.get().checked_add(1).ok_or_else(|| {
                        DetachedPermissionAdmissionError::Denied(
                            "detached permission response route epoch exhausted".to_string(),
                        )
                    })?,
                )
                .map_err(|_| {
                    DetachedPermissionAdmissionError::Denied(
                        "detached permission response route epoch is invalid".to_string(),
                    )
                })?;
                let batch = self.surface_event_batch_with_commit_id(
                    vec![(
                        surface::SurfaceScope::Thread,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged {
                                interaction_id: mailbox.interaction_id.clone(),
                                expected_revision: revision,
                                next_revision,
                                route: surface::SurfaceInteractionRoute::Unassigned {
                                    epoch: next_epoch,
                                },
                            },
                        ),
                    )],
                    None,
                );
                self.resident_surface
                    .coordinator
                    .commit_actor_batch(&batch)
                    .map_err(|error| {
                        DetachedPermissionAdmissionError::Deferred(format!(
                            "detached permission route repark deferred: {error:?}"
                        ))
                    })?;
                revision = next_revision;
                surface::BrokerInteractionResponseRoute::Unassigned { epoch: next_epoch }
            }
        };
        let record = surface::BrokerInteractionRequestRecord {
            thread_id: view.fence.thread_id.clone(),
            interaction_id: view.interaction_id.clone(),
            fence: view.fence.clone(),
            kind: view.kind,
            request: view.request.clone(),
            response_token: surface::SurfaceResponseToken::new(random_token_bytes()),
            answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
            recovery_disposition: view.recovery_disposition.clone(),
        };
        self.resident_surface.interactions.insert(
            mailbox.interaction_id.clone(),
            ResidentSurfaceInteraction {
                record,
                route,
                revision,
                waiter: Some(ResidentInteractionWaiter::DetachedPermission {
                    registry: registry.clone(),
                    key: mailbox.key.clone(),
                    request_digest: mailbox.request_digest.clone(),
                }),
                private_response: None,
                pending_background_route: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    /// Build a surface tool identity for a child capability request. Child
    /// tools are not provider tools and therefore must never be recovered by
    /// looking in `snapshot.tools`; that table is reserved for model response
    /// tool calls. The resulting identity is only used to bind the durable
    /// interaction/capsule to the child turn and request id.
    fn surface_child_permission_tool(
        request: &crate::runtime_permission::RuntimePermissionRequest,
    ) -> io::Result<surface::SurfaceToolRequest> {
        use crate::runtime_permission::RuntimePermissionContext;

        let RuntimePermissionContext::Child {
            turn_id,
            tool_call_id,
            ..
        } = &request.context
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child permission tool requires a child owner context",
            ));
        };
        if request.id != tool_call_id.as_str() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "child permission request id does not match its tool owner",
            ));
        }
        let name = surface::NonEmptyText::try_new("child-permission")
            .expect("child permission display name is non-empty");
        let action = if request.permissions.network.is_some() {
            surface::SurfaceToolAction::Network
        } else if request
            .permissions
            .file_system
            .as_ref()
            .is_some_and(|permissions| {
                permissions
                    .write
                    .as_ref()
                    .is_some_and(|paths| !paths.is_empty())
            })
        {
            surface::SurfaceToolAction::Write
        } else if request
            .permissions
            .file_system
            .as_ref()
            .is_some_and(|permissions| {
                permissions
                    .read
                    .as_ref()
                    .is_some_and(|paths| !paths.is_empty())
            })
        {
            surface::SurfaceToolAction::Read
        } else {
            // An empty/extension capability request still needs a stable
            // action for the authority fingerprint. Shell is the existing
            // generic effect-bearing action and does not grant anything by
            // itself; the requested profile remains the capability boundary.
            surface::SurfaceToolAction::Shell
        };
        let raw_arguments = surface::DisplayText::new("{}");
        let arguments_digest = surface_sha256(raw_arguments.as_str().as_bytes());
        Ok(surface::SurfaceToolRequest {
            tool_call_id: tool_call_id.clone(),
            source_response_id: None,
            turn_id: turn_id.clone(),
            name,
            action,
            target: request.reason.clone().map(surface::DisplayText::new),
            raw_arguments,
            arguments_digest,
        })
    }

    fn surface_permission_context_for_tool(
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        tool: &surface::SurfaceToolRequest,
        supplied: &crate::runtime_permission::RuntimePermissionContext,
    ) -> io::Result<surface::SurfacePermissionContext> {
        use crate::runtime_permission::RuntimePermissionContext;

        match supplied {
            RuntimePermissionContext::Foreground { origin } => {
                if *origin == surface::SurfacePermissionOrigin::ChildAgent {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "child-agent origin requires a complete child permission owner",
                    ));
                }
                Ok(surface::SurfacePermissionContext::foreground_for_tool(
                    tool, *origin,
                ))
            }
            RuntimePermissionContext::Child {
                task_id,
                task_revision,
                agent_id,
                agent_revision,
                activity_id,
                turn_id,
                tool_call_id,
                origin,
            } => {
                if *origin != surface::SurfacePermissionOrigin::ChildAgent {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "child permission owner must use the child-agent origin",
                    ));
                }
                if turn_id != &tool.turn_id || tool_call_id != &tool.tool_call_id {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "child permission context does not own the committed tool identity",
                    ));
                }
                // Activity ids are produced at a tool activity boundary. Until the
                // child relay supplies an independent activity ledger, require the
                // activity to be exactly the immutable provider tool-call id.
                if activity_id.as_str() != tool.tool_call_id.as_str() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "child permission activity is stale or does not match the tool call",
                    ));
                }
                let task = snapshot
                    .tasks
                    .iter()
                    .find(|task| task.task_id == *task_id)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "child permission task is not committed",
                        )
                    })?;
                let subagent = snapshot
                    .subagents
                    .iter()
                    .find(|subagent| subagent.subagent_id == *agent_id)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "child permission subagent is not committed",
                        )
                    })?;
                // `task_revision` is the child owner's admission floor. The
                // mutable task CAS revision can be higher because opening or
                // resolving an earlier permission prompt also advances it.
                let detached_owner = matches!(
                    &subagent.owner,
                    surface::SurfaceSubagentOwner::DetachedTask { .. }
                );
                let agent_revision_matches = if detached_owner {
                    subagent.revision >= *agent_revision
                } else {
                    subagent.revision == *agent_revision
                };
                if task.revision < *task_revision
                    || task.status != surface::SurfaceTaskStatus::Running
                    || task.pending_interaction_id.is_some()
                    || !agent_revision_matches
                    || task.subagent_id.as_ref() != Some(agent_id)
                    || subagent.task_id != *task_id
                    || subagent.source.turn_id != *turn_id
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "child permission owner epoch or generation is stale",
                    ));
                }
                match &subagent.owner {
                    surface::SurfaceSubagentOwner::Generation { fence: owner }
                        if owner == fence
                            && task.parent_operation.as_ref() == Some(&fence.operation_id) => {}
                    surface::SurfaceSubagentOwner::DetachedTask { owner }
                        if owner.task_id == *task_id
                            && owner.task_revision.get() <= task.revision.get()
                            && subagent.source.attempt_id == owner.attempt_id
                            && task.parent_operation.as_ref() == Some(&fence.operation_id) => {}
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "child permission owner epoch or generation is stale",
                        ));
                    }
                }
                Ok(surface::SurfacePermissionContext {
                    owner: surface::SurfacePermissionOwnerRef::Child {
                        task_id: task_id.clone(),
                        // Normalize the public interaction to the actor's
                        // current task CAS revision. The worker-provided
                        // floor remains only an admission check above.
                        task_revision: task.revision,
                        agent_id: agent_id.clone(),
                        agent_revision: *agent_revision,
                        activity_id: activity_id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                    },
                    origin: *origin,
                })
            }
        }
    }

    /// Build the one persistent settings snapshot that represents a session
    /// permission grant. This accepts only capabilities the runtime can carry
    /// across turns without widening a read-only or protected-metadata grant.
    fn surface_session_permission_settings(
        &self,
        permissions: &surface::SurfacePermissionProfile,
    ) -> Result<Option<(surface::SurfaceSettingsSnapshot, RunConfig)>, &'static str> {
        let current = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .settings
            .clone();
        let mut next = current.clone();
        let mut directories = next.effective.additional_working_directories.clone();

        if let Some(file_system) = permissions.file_system.as_ref() {
            if file_system
                .read
                .as_ref()
                .is_some_and(|paths| !paths.is_empty())
            {
                return Err("session scope cannot persist read-only filesystem grants");
            }
            for path in file_system.write.iter().flatten() {
                let path = surface::CanonicalPath::try_new(PathBuf::from(path.0.as_str()))
                    .map_err(|_| "session scope requires canonical absolute filesystem paths")?;
                if orca_tools::sandbox::is_protected_metadata_root(path.as_path()) {
                    return Err("protected metadata paths are limited to the current turn");
                }
                if !directories.iter().any(|directory| directory.path == path) {
                    directories.push(surface::SurfaceAdditionalWorkingDirectory {
                        path,
                        source: surface::NonEmptyText::try_new("session")
                            .expect("session permission source is non-empty"),
                    });
                }
            }
        }

        if permissions
            .network
            .as_ref()
            .is_some_and(|network| network.enabled.is_some() || !network.domains.is_empty())
        {
            return Err("session scope cannot persist network grants without a runtime policy");
        }
        let network = next.effective.network_permissions.clone();

        if directories == current.effective.additional_working_directories
            && network == current.effective.network_permissions
        {
            return Ok(None);
        }

        next.effective.additional_working_directories = directories;
        next.effective.network_permissions = network;
        next.thread_revision = surface::SettingsRevision::try_new(
            current
                .thread_revision
                .get()
                .checked_add(1)
                .ok_or("session permission settings revision overflow")?,
        )
        .map_err(|_| "session permission settings revision is invalid")?;
        next.effective.policy_epoch = surface::PolicyEpoch::try_new(
            current
                .effective
                .policy_epoch
                .get()
                .checked_add(1)
                .ok_or("session permission policy epoch overflow")?,
        )
        .map_err(|_| "session permission policy epoch is invalid")?;
        next.pending = None;

        let mut next_config = self.config.clone();
        next_config.additional_working_directories = next
            .effective
            .additional_working_directories
            .iter()
            .map(|directory| orca_core::config::AdditionalWorkingDirectory {
                path: directory.path.as_path().to_path_buf(),
                source: directory.source.as_str().to_string(),
            })
            .collect();
        Ok(Some((next, next_config)))
    }

    pub(super) fn surface_authority_for_tool(
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        tool: &surface::SurfaceToolRequest,
    ) -> io::Result<surface::AuthorityFingerprint> {
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let surface::Replayability::Replayable {
            request_digest: Some(request_digest),
            cwd,
            workspace_roots,
            policy_epoch,
            tool_schema_digest,
            ..
        } = &generation.replayability
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect-bearing interactions require a replayable generation authority",
            ));
        };
        if *policy_epoch != operation.intent.policy_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect-bearing interaction policy epoch is stale",
            ));
        }
        let executable_generation = surface_sha256(
            &serde_json::to_vec(tool)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        let workspace_roots_digest = surface_sha256(
            &serde_json::to_vec(workspace_roots)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        Ok(surface::AuthorityFingerprint::new(
            fence.operation_id.clone(),
            request_digest.clone(),
            tool_schema_digest.clone(),
            cwd.clone(),
            workspace_roots_digest,
            *policy_epoch,
            executable_generation,
            tool.arguments_digest.clone(),
            generation.capability_fingerprint.clone(),
        ))
    }

    /// Function intent contract:
    ///
    /// - Input: an active generation, preallocated stable interaction id,
    ///   typed effect request, and its already-decided recovery disposition.
    /// - Output: durably commits the exact request before presentation and
    ///   returns the private broker record/route for the live waiter.
    /// - Errors: rejects stale or terminalizing generations and preserves the
    ///   existing capability-unavailable fail-closed behavior.
    /// - State changes: writes one interaction request batch; it never builds
    ///   or interprets a continuation capsule itself.
    pub(super) fn commit_surface_effect_interaction_request(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        interaction_id: surface::SurfaceInteractionId,
        kind: surface::SurfaceInteractionKind,
        request: surface::SurfaceInteractionRequest,
        recovery_disposition: surface::InteractionUnavailableDisposition,
    ) -> io::Result<
        Option<(
            surface::SurfaceInteractionId,
            surface::BrokerInteractionRequestRecord,
            surface::BrokerInteractionResponseRoute,
            surface::InteractionRevision,
        )>,
    > {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect interaction generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let preferred = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self
            .resident_surface
            .hub
            .select_interaction_attachment_for(kind, preferred);
        let PreparedInteractionRequest {
            interaction_id,
            record,
            route,
            revision,
            mut events,
            unavailable,
        } = prepare_interaction_request(
            fence.clone(),
            interaction_id,
            kind,
            request,
            recovery_disposition,
            attachment_id,
        );
        let child_permission_task = match &record.request {
            surface::SurfaceInteractionRequest::PermissionRequest { context, .. }
                if !unavailable =>
            {
                context.child_owner_task()
            }
            _ => None,
        };
        if let Some((task_id, expected_revision)) = child_permission_task {
            let task = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .tasks
                .iter()
                .find(|task| task.task_id == *task_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "permission task missing")
                })?;
            let next_revision = surface::TaskRevision::try_new(
                expected_revision
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("permission task revision exhausted"))?,
            )
            .map_err(|_| io::Error::other("permission task revision is invalid"))?;
            if task.revision != expected_revision
                || task.status != surface::SurfaceTaskStatus::Running
                || task.pending_interaction_id.is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "permission task request epoch is stale",
                ));
            }
            events.push((
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::InteractionChanged {
                    task_id: task_id.clone(),
                    expected_revision,
                    next_revision,
                    status: surface::SurfaceTaskStatus::ApprovalRequired,
                    pending_interaction_id: Some(interaction_id.clone()),
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        let committed = if child_permission_task.is_some() {
            self.resident_surface
                .coordinator
                .commit_actor_generation_permission_request_batch(fence, &batch)
        } else {
            self.resident_surface
                .coordinator
                .commit_generation_batch(fence, &batch)
        };
        committed.map_err(|error| {
            io::Error::other(format!("failed to commit effect interaction: {error:?}"))
        })?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            active.surface_execution_failure_diagnostic = None;
            return Ok(None);
        }
        Ok(Some((interaction_id, record, route, revision)))
    }

    pub(super) fn request_surface_tool_approval(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        approval: orca_core::approval_types::ApprovalRequest,
        request: orca_core::tool_types::ToolRequest,
        reply: SyncSender<io::Result<orca_core::approval_types::ApprovalResolution>>,
    ) {
        let result = (|| -> io::Result<()> {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            let jsonl_compatibility_fallback = snapshot
                .foreground_operation
                .as_ref()
                .filter(|operation| operation.operation_id == fence.operation_id)
                .is_some_and(|operation| {
                    matches!(
                        operation.intent.origin,
                        surface::OperationOrigin::Headless
                            | surface::OperationOrigin::JsonlThreadTurn { .. }
                    )
                });
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let authority = Self::surface_authority_for_tool(&snapshot, &fence, &tool)?;
            let interaction_request = surface::SurfaceInteractionRequest::ToolApproval {
                tool: tool.clone(),
                description: surface::DisplayText::new(approval.description.clone()),
                preview: approval.preview.clone().map(surface::DisplayText::new),
                authority: authority.clone(),
            };
            let interaction_id =
                surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let execution_context_fingerprint =
                cold_recovery_thread_config_fingerprint(&self.config, &snapshot);
            let capsule = surface::DurableInteractionContinuationCapsule::try_new_restartable(
                interaction_id.clone(),
                fence.clone(),
                interaction_request.clone(),
                execution_context_fingerprint,
                surface::DurableInteractionContinuationIntent::ToolInvocation(
                    surface::ToolInvocationIntent::before_invocation(tool, authority),
                ),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let recovery_disposition =
                surface::InteractionUnavailableDisposition::restartable_tool_approval(&capsule)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let Some((interaction_id, record, route, revision)) = self
                .commit_surface_effect_interaction_request(
                    active,
                    fence,
                    interaction_id,
                    surface::SurfaceInteractionKind::ToolApproval,
                    interaction_request,
                    recovery_disposition,
                )?
            else {
                if jsonl_compatibility_fallback {
                    active.surface_execution_failure =
                        Some(surface::GenerationExecutionFailureClass::LegacyApprovalRequired);
                    active.surface_execution_failure_diagnostic = None;
                }
                let _ = reply.send(Ok(orca_core::approval_types::ApprovalResolution {
                    id: approval.id,
                    decision: orca_core::approval_types::ApprovalDecision::Deny,
                    reason: "no runtime surface can answer tool approval".to_string(),
                }));
                return Ok(());
            };
            self.resident_surface.interactions.insert(
                interaction_id,
                ResidentSurfaceInteraction {
                    record,
                    route,
                    revision,
                    waiter: Some(ResidentInteractionWaiter::ToolApproval {
                        approval_id: approval.id,
                        waiter: reply.clone(),
                    }),
                    private_response: None,
                    pending_background_route: None,
                    winning_receipt: None,
                    resolution_ack: None,
                    projected_cursor: None,
                    cancelled: None,
                },
            );
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn request_surface_permission(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: crate::runtime_permission::RuntimePermissionRequest,
        retry_overlay: Option<crate::runtime_permission::TurnPermissionOverlay>,
        reply: SyncSender<io::Result<crate::runtime_permission::RuntimePermissionResponse>>,
    ) {
        let result = (|| -> io::Result<()> {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            // Provider requests are bound to the committed model tool table;
            // child requests are bound to their durable task/subagent source
            // and intentionally do not require (or fabricate) a provider
            // `SurfaceToolView`.
            let (tool_request, context) = match &request.context {
                crate::runtime_permission::RuntimePermissionContext::Foreground { .. } => {
                    let tool_call_id = surface::SurfaceToolCallId::try_new(request.id.clone())
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "empty permission request id",
                            )
                        })?;
                    let tool_request = snapshot
                        .tools
                        .iter()
                        .find(|tool| tool.request.tool_call_id == tool_call_id)
                        .map(|tool| tool.request.clone())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "permission interaction lacks a committed provider tool identity",
                            )
                        })?;
                    let context = Self::surface_permission_context_for_tool(
                        snapshot,
                        &fence,
                        &tool_request,
                        &request.context,
                    )?;
                    (tool_request, context)
                }
                crate::runtime_permission::RuntimePermissionContext::Child { .. } => {
                    let tool_request = Self::surface_child_permission_tool(&request)?;
                    let context = Self::surface_permission_context_for_tool(
                        snapshot,
                        &fence,
                        &tool_request,
                        &request.context,
                    )?;
                    (tool_request, context)
                }
            };
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let generation = operation
                .generations
                .iter()
                .find(|generation| generation.fence == fence)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "generation missing"))?;
            if matches!(
                &request.context,
                crate::runtime_permission::RuntimePermissionContext::Foreground { .. }
            ) && (tool_request.source_response_id.is_none()
                || tool_request.turn_id != generation.logical_turn_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "permission request is not bound to the current provider tool",
                ));
            }
            let authority = Self::surface_authority_for_tool(&snapshot, &fence, &tool_request)?;
            let permissions = surface_permission_profile_from_runtime(request.permissions.clone())?;
            let interaction_request = surface::SurfaceInteractionRequest::PermissionRequest {
                tool_call_id: tool_request.tool_call_id.clone(),
                context,
                reason: request.reason.clone().map(surface::DisplayText::new),
                permissions: permissions.clone(),
                authority: authority.clone(),
            };
            let interaction_id =
                surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let recovery_disposition = match retry_overlay {
                Some(retry_overlay) => surface_permission_retry_overlay_from_runtime(
                    &retry_overlay,
                )
                .ok()
                .and_then(|permission_overlay| {
                    surface::DurableInteractionContinuationCapsule::try_new_restartable(
                        interaction_id.clone(),
                        fence.clone(),
                        interaction_request.clone(),
                        cold_recovery_thread_config_fingerprint(&self.config, &snapshot),
                        surface::DurableInteractionContinuationIntent::PermissionRetry(
                            surface::PermissionRetryIntent::pre_side_effect(
                                tool_request,
                                permissions,
                                permission_overlay,
                                authority,
                            ),
                        ),
                    )
                    .ok()
                })
                .and_then(|capsule| {
                    surface::InteractionUnavailableDisposition::restartable_permission_request(
                        &capsule,
                    )
                    .ok()
                })
                .unwrap_or(surface::InteractionUnavailableDisposition::FailOperation),
                None => surface::InteractionUnavailableDisposition::FailOperation,
            };
            let Some((interaction_id, record, route, revision)) = self
                .commit_surface_effect_interaction_request(
                    active,
                    fence,
                    interaction_id,
                    surface::SurfaceInteractionKind::PermissionRequest,
                    interaction_request,
                    recovery_disposition,
                )?
            else {
                let _ = reply.send(Ok(crate::runtime_permission::RuntimePermissionResponse {
                    decision: crate::protocol::PermissionResponseDecision::Deny,
                    scope: crate::protocol::PermissionGrantScope::Turn,
                    permissions: request.permissions,
                    strict_auto_review: false,
                }));
                return Ok(());
            };
            self.resident_surface.interactions.insert(
                interaction_id,
                ResidentSurfaceInteraction {
                    record,
                    route,
                    revision,
                    waiter: Some(ResidentInteractionWaiter::Permission(reply.clone())),
                    private_response: None,
                    pending_background_route: None,
                    winning_receipt: None,
                    resolution_ack: None,
                    projected_cursor: None,
                    cancelled: None,
                },
            );
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn request_surface_user_input(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: crate::lifecycle::RuntimeUserInputRequest,
        reply: SyncSender<io::Result<Option<String>>>,
    ) {
        let result = self.request_surface_user_input_inner(active, fence, &request, reply.clone());
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn request_surface_user_input_inner(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: &crate::lifecycle::RuntimeUserInputRequest,
        reply: SyncSender<io::Result<Option<String>>>,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "user-input request generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let request_identity = surface::NonEmptyText::try_new(request.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty user-input id"))?;
        let question = surface::NonEmptyText::try_new(request.question.clone()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty user-input question")
        })?;
        let preferred = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self.resident_surface.hub.select_interaction_attachment_for(
            surface::SurfaceInteractionKind::UserInput,
            preferred,
        );
        let interaction_id =
            surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let interaction_request = surface::SurfaceInteractionRequest::UserInput {
            question: question.clone(),
            suggestions: request
                .choices
                .iter()
                .cloned()
                .map(surface::DisplayText::new)
                .collect(),
        };
        let continuation_intent = surface::ContinuationTurnIntent::user_input(
            request_identity,
            question,
            request
                .choices
                .iter()
                .cloned()
                .map(surface::DisplayText::new)
                .collect(),
        );
        let capsule = surface::DurableInteractionContinuationCapsule::try_new_restartable(
            interaction_id.clone(),
            fence.clone(),
            interaction_request.clone(),
            cold_recovery_thread_config_fingerprint(
                &self.config,
                self.resident_surface.coordinator.state().snapshot(),
            ),
            surface::DurableInteractionContinuationIntent::ContinuationTurn(continuation_intent),
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let recovery_disposition =
            surface::InteractionUnavailableDisposition::restartable_continuation_turn(&capsule)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let PreparedInteractionRequest {
            interaction_id,
            record,
            route,
            revision,
            events,
            unavailable,
        } = prepare_interaction_request(
            fence.clone(),
            interaction_id,
            surface::SurfaceInteractionKind::UserInput,
            interaction_request,
            recovery_disposition,
            attachment_id,
        );
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &batch)
            .map_err(|error| {
                io::Error::other(format!("failed to commit user-input request: {error:?}"))
            })?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            active.surface_execution_failure_diagnostic = None;
            let _ = reply.send(Ok(None));
            return Ok(());
        }
        self.resident_surface.interactions.insert(
            interaction_id.clone(),
            ResidentSurfaceInteraction {
                record,
                route,
                revision,
                waiter: Some(ResidentInteractionWaiter::UserInput(reply)),
                private_response: None,
                pending_background_route: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    pub(super) fn request_surface_mcp_elicitation(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_mcp::McpElicitationRequest,
        reply: SyncSender<Result<orca_mcp::McpElicitationResponse, String>>,
    ) {
        let result =
            self.request_surface_mcp_elicitation_inner(active, fence, &request, reply.clone());
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn request_surface_mcp_elicitation_inner(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: &orca_mcp::McpElicitationRequest,
        reply: SyncSender<Result<orca_mcp::McpElicitationResponse, String>>,
    ) -> Result<(), String> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err("MCP elicitation generation fence is stale".to_string());
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err("runtime generation is terminalizing".to_string());
        }
        let opaque_request_id = surface::NonEmptyText::try_new(request.id.clone())
            .map_err(|_| "empty MCP elicitation id".to_string())?;
        let server_name = surface::NonEmptyText::try_new(request.server_name.clone())
            .map_err(|_| "empty MCP server name".to_string())?;
        let requested_schema = request
            .requested_schema
            .as_ref()
            .map(surface_data_from_json)
            .transpose()?;
        let mcp_request = match request.mode {
            orca_mcp::McpElicitationMode::Form => surface::SurfaceMcpElicitationRequest::Form {
                requested_schema,
                supported_schema: None,
            },
            orca_mcp::McpElicitationMode::Url => surface::SurfaceMcpElicitationRequest::Url {
                raw_url: request.url.clone().map(surface::DisplayText::new),
                requested_schema,
            },
        };
        let preferred = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self.resident_surface.hub.select_interaction_attachment_for(
            surface::SurfaceInteractionKind::McpElicitation,
            preferred,
        );
        let interaction_id =
            surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let interaction_request = surface::SurfaceInteractionRequest::McpElicitation {
            server_name: server_name.clone(),
            server_request_id: opaque_request_id.clone(),
            message: surface::DisplayText::new(request.message.clone()),
            request: mcp_request.clone(),
        };
        let continuation_intent = surface::ContinuationTurnIntent::mcp_elicitation(
            server_name,
            opaque_request_id,
            surface::DisplayText::new(request.message.clone()),
            mcp_request,
        );
        let capsule = surface::DurableInteractionContinuationCapsule::try_new_restartable(
            interaction_id.clone(),
            fence.clone(),
            interaction_request.clone(),
            cold_recovery_thread_config_fingerprint(
                &self.config,
                self.resident_surface.coordinator.state().snapshot(),
            ),
            surface::DurableInteractionContinuationIntent::ContinuationTurn(continuation_intent),
        )
        .map_err(|error| error.to_string())?;
        let recovery_disposition =
            surface::InteractionUnavailableDisposition::restartable_continuation_turn(&capsule)
                .map_err(|error| error.to_string())?;
        let PreparedInteractionRequest {
            interaction_id,
            record,
            route,
            revision,
            events,
            unavailable,
        } = prepare_interaction_request(
            fence.clone(),
            interaction_id,
            surface::SurfaceInteractionKind::McpElicitation,
            interaction_request,
            recovery_disposition,
            attachment_id,
        );
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &batch)
            .map_err(|error| format!("failed to commit MCP elicitation request: {error:?}"))?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            active.surface_execution_failure_diagnostic = None;
            let _ = reply.send(Ok(orca_mcp::McpElicitationResponse::Decline));
            return Ok(());
        }
        self.resident_surface.interactions.insert(
            interaction_id.clone(),
            ResidentSurfaceInteraction {
                record,
                route,
                revision,
                waiter: Some(ResidentInteractionWaiter::McpElicitation(reply)),
                private_response: None,
                pending_background_route: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    pub(super) fn project_headless_interaction(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        interaction_id: surface::SurfaceInteractionId,
    ) -> Result<Option<HeadlessInteractionCheckpoint>, surface::SurfaceClientCommandError> {
        if client.grant().role != surface::SurfaceAttachmentRole::Headless {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !self
            .resident_surface
            .interactions
            .contains_key(&interaction_id)
        {
            return Ok(None);
        }
        self.assign_unassigned_background_interaction(client, &interaction_id)?;
        let interaction = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if interaction.cancelled.is_some()
            || interaction.winning_receipt.is_some()
            || !self
                .resident_surface
                .hub
                .admits_interaction_client(client, interaction.record.kind)
        {
            return Ok(None);
        }
        let selector = exact_interaction_selectors(interaction)
            .into_iter()
            .find_map(|(attachment_id, selector)| {
                (attachment_id == *client.attachment_id()).then_some(selector)
            })
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let route = match &interaction.route {
            surface::BrokerInteractionResponseRoute::Unassigned { epoch } => {
                surface::SurfaceInteractionRoute::Unassigned { epoch: *epoch }
            }
            surface::BrokerInteractionResponseRoute::Exclusive {
                epoch,
                attachment_id,
                ..
            } => surface::SurfaceInteractionRoute::Exclusive {
                epoch: *epoch,
                attachment_id: attachment_id.clone(),
            },
            surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { epoch, grants } => {
                surface::SurfaceInteractionRoute::SharedFirstCommitWins {
                    epoch: *epoch,
                    attachments: surface::NonEmptySet::try_new(
                        grants
                            .as_slice()
                            .iter()
                            .map(|(attachment_id, _)| attachment_id.clone())
                            .collect(),
                    )
                    .expect("private interaction route grants are non-empty"),
                }
            }
        };
        Ok(Some(HeadlessInteractionCheckpoint {
            interaction: surface::SurfaceInteractionView {
                interaction_id: interaction.record.interaction_id.clone(),
                revision: interaction.revision,
                fence: interaction.record.fence.clone(),
                kind: interaction.record.kind,
                request: interaction.record.request.clone(),
                route,
                lifecycle: surface::SurfaceInteractionLifecycle::Requested,
                recovery_disposition: interaction.record.recovery_disposition.clone(),
            },
            selector,
        }))
    }

    pub(super) fn respond_surface_interaction_by_id(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.respond_surface_interaction_by_id_with_policy(
            client,
            request_id,
            interaction_id,
            answer,
            surface::BrokerInteractionAnswerPolicy::NativeStrict,
        )
    }

    pub(super) fn respond_surface_interaction_by_id_with_policy(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
        policy: surface::BrokerInteractionAnswerPolicy,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        if let Some(interaction) = self.resident_surface.interactions.get(&interaction_id)
            && let Some(winning_receipt) = interaction.winning_receipt.clone()
        {
            if !self
                .resident_surface
                .hub
                .admits_interaction_client(client, interaction.record.kind)
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            let acknowledgement = interaction
                .resolution_ack
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![acknowledgement])
                        .expect("interaction replay has one acknowledgement"),
                },
                value: surface::RespondInteractionOutput {
                    interaction_id,
                    attempted_response_id: winning_receipt.response_id.clone(),
                    disposition: surface::RespondInteractionDisposition::AlreadyResolved {
                        winning_receipt,
                    },
                    projected_cursor: interaction.projected_cursor.clone(),
                },
            });
        }
        self.assign_unassigned_background_interaction(client, &interaction_id)?;
        let interaction = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        if interaction.cancelled.is_some() || interaction.winning_receipt.is_some() {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let expected_kind = interaction.record.kind;
        if !self
            .resident_surface
            .hub
            .admits_interaction_client(client, expected_kind)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let selector = exact_interaction_selectors(interaction)
            .into_iter()
            .find_map(|(attachment_id, selector)| {
                (attachment_id == *client.attachment_id()).then_some(selector)
            })
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let response_id = interaction
            .private_response
            .as_ref()
            .map(|winner| winner.record.receipt.response_id.clone())
            .unwrap_or_else(|| {
                surface::SurfaceResponseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7")
            });
        let authority = interaction_answer_authority(&interaction.record.request, &answer);
        let response =
            surface::BoundInteractionResponse::new(response_id, answer, policy, authority);
        self.respond_surface_interaction(client, request_id, selector, response)
    }

    pub(super) fn assign_unassigned_background_interaction(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        if self
            .resident_surface
            .interactions
            .get(interaction_id)
            .is_some_and(|interaction| interaction.pending_background_route.is_some())
        {
            self.retry_background_interaction_route(interaction_id);
            if self
                .resident_surface
                .interactions
                .get(interaction_id)
                .is_some_and(|interaction| interaction.pending_background_route.is_some())
            {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        }
        let Some(interaction) = self.resident_surface.interactions.get(interaction_id) else {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        };
        if !matches!(
            interaction.route,
            surface::BrokerInteractionResponseRoute::Unassigned { .. }
        ) {
            return Ok(());
        }
        // Detached child permission prompts are thread-owned. They may have
        // been created while no attachment was connected, so assigning one
        // later must use the actor's Thread authority rather than a retired
        // generation publisher permit.
        let detached_permission = {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            is_detached_child_permission_interaction(snapshot, interaction)
        };
        if detached_permission {
            if interaction.record.kind != surface::SurfaceInteractionKind::PermissionRequest
                || !self
                    .resident_surface
                    .hub
                    .admits_interaction_client(client, interaction.record.kind)
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            let expected_revision = interaction.revision;
            let next_revision = surface::InteractionRevision::try_new(
                expected_revision
                    .get()
                    .checked_add(1)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let next_epoch = surface::ResponseRouteEpoch::try_new(
                interaction_route_epoch(&interaction.route)
                    .get()
                    .checked_add(1)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let public_route = surface::SurfaceInteractionRoute::Exclusive {
                epoch: next_epoch,
                attachment_id: client.attachment_id().clone(),
            };
            let private_route = surface::BrokerInteractionResponseRoute::Exclusive {
                epoch: next_epoch,
                attachment_id: client.attachment_id().clone(),
                grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
            };
            let batch = self.surface_event_batch_with_commit_id(
                vec![(
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::RouteChanged {
                        interaction_id: interaction_id.clone(),
                        expected_revision,
                        next_revision,
                        route: public_route,
                    }),
                )],
                None,
            );
            self.resident_surface
                .coordinator
                .commit_actor_batch(&batch)
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let interaction = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
            interaction.revision = next_revision;
            interaction.route = private_route;
            return Ok(());
        }
        if self
            .resident_surface
            .interactions
            .cold_recovery_owners
            .contains_key(interaction_id)
            || self
                .resident_surface
                .interactions
                .cold_recovery_permission_owners
                .contains_key(interaction_id)
            || self
                .resident_surface
                .interactions
                .continuation_turn_owners
                .contains_key(interaction_id)
        {
            if !matches!(
                interaction.record.kind,
                surface::SurfaceInteractionKind::ToolApproval
                    | surface::SurfaceInteractionKind::PermissionRequest
                    | surface::SurfaceInteractionKind::UserInput
                    | surface::SurfaceInteractionKind::McpElicitation
            ) || !self
                .resident_surface
                .hub
                .admits_interaction_client(client, interaction.record.kind)
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            let expected_revision = interaction.revision;
            let next_revision = surface::InteractionRevision::try_new(
                expected_revision
                    .get()
                    .checked_add(1)
                    .expect("interaction revision did not exhaust"),
            )
            .expect("interaction revision did not exhaust");
            let current_epoch = interaction_route_epoch(&interaction.route);
            let next_epoch =
                surface::ResponseRouteEpoch::try_new(current_epoch.get().saturating_add(1))
                    .expect("interaction route epoch did not exhaust");
            let public_route = surface::SurfaceInteractionRoute::Exclusive {
                epoch: next_epoch,
                attachment_id: client.attachment_id().clone(),
            };
            let private_route = surface::BrokerInteractionResponseRoute::Exclusive {
                epoch: next_epoch,
                attachment_id: client.attachment_id().clone(),
                grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
            };
            let fence = interaction.record.fence.clone();
            let batch = self.surface_event_batch_with_commit_id(
                vec![(
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::RouteChanged {
                        interaction_id: interaction_id.clone(),
                        expected_revision,
                        next_revision,
                        route: public_route,
                    }),
                )],
                None,
            );
            self.resident_surface
                .coordinator
                .commit_generation_batch(fence, &batch)
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let interaction = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .expect("cold-recovery ToolApproval remains resident");
            interaction.revision = next_revision;
            interaction.route = private_route;
            return Ok(());
        }
        if interaction.record.kind != surface::SurfaceInteractionKind::BackgroundApproval
            || !self.resident_surface.hub.admits_interaction_client(
                client,
                surface::SurfaceInteractionKind::BackgroundApproval,
            )
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let background_fence = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .background_operations
            .iter()
            .find(|background| {
                background.operation_id == interaction.record.fence.operation_id
                    && background.fence.operation_fence == interaction.record.fence
            })
            .map(|background| background.fence.clone())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let expected_revision = interaction.revision;
        let next_revision =
            surface::InteractionRevision::try_new(expected_revision.get().saturating_add(1))
                .expect("interaction revision did not exhaust");
        let current_epoch = interaction_route_epoch(&interaction.route);
        let next_epoch =
            surface::ResponseRouteEpoch::try_new(current_epoch.get().saturating_add(1))
                .expect("interaction route epoch did not exhaust");
        let grant_token = surface::SurfaceResponseGrantToken::new(random_token_bytes());
        let public_route = surface::SurfaceInteractionRoute::Exclusive {
            epoch: next_epoch,
            attachment_id: client.attachment_id().clone(),
        };
        let private_route = surface::BrokerInteractionResponseRoute::Exclusive {
            epoch: next_epoch,
            attachment_id: client.attachment_id().clone(),
            grant_token,
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Background {
                    fence: background_fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::RouteChanged {
                    interaction_id: interaction_id.clone(),
                    expected_revision,
                    next_revision,
                    route: public_route,
                }),
            )],
            None,
        );
        let interaction = self
            .resident_surface
            .interactions
            .get_mut(interaction_id)
            .expect("background interaction remains resident");
        interaction.pending_background_route = Some(PendingBackgroundInteractionRoute {
            fence: background_fence,
            batch,
            next_revision,
            private_route,
            retry_at: tokio::time::Instant::now(),
        });
        self.retry_background_interaction_route(interaction_id);
        if self
            .resident_surface
            .interactions
            .get(interaction_id)
            .is_some_and(|interaction| interaction.pending_background_route.is_some())
        {
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        } else {
            Ok(())
        }
    }

    pub(super) fn retry_background_interaction_route(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) {
        let Some(pending) = self
            .resident_surface
            .interactions
            .get(interaction_id)
            .and_then(|interaction| interaction.pending_background_route.clone())
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_provider_background_interaction_route_batch(
                pending.fence.clone(),
                &pending.batch,
            )
            .is_err()
        {
            if let Some(route) = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .and_then(|interaction| interaction.pending_background_route.as_mut())
            {
                route.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        let interaction = self
            .resident_surface
            .interactions
            .get_mut(interaction_id)
            .expect("committed background interaction remains resident");
        interaction.revision = pending.next_revision;
        interaction.route = pending.private_route;
        interaction.pending_background_route = None;
    }

    pub(super) fn respond_surface_interaction(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        selector: surface::InteractionSelector,
        response: surface::BoundInteractionResponse,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self
            .resident_surface
            .interactions
            .pending_detaches
            .is_empty()
            || !self
                .resident_surface
                .interactions
                .pending_capability_losses
                .is_empty()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let (interaction_id, expected_kind, exact) = match selector {
            surface::InteractionSelector::OpaqueRequestId { .. } => {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            surface::InteractionSelector::Exact {
                interaction_id,
                expected_revision,
                kind,
                response_token,
                response_route_epoch,
                response_grant_token,
                operation_fence,
            } => (
                interaction_id,
                kind,
                ExactInteractionSelectorBinding {
                    expected_revision,
                    response_token,
                    route_epoch: response_route_epoch,
                    grant_token: response_grant_token,
                    operation_fence,
                },
            ),
        };
        let interaction = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let interaction = &interaction;
        {
            if interaction.revision != exact.expected_revision {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleRevision,
                    "interaction revision is stale",
                ));
            }
            if interaction.record.fence != exact.operation_fence {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleFence,
                    "interaction operation fence is stale",
                ));
            }
            if interaction.record.response_token != exact.response_token {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::WrongResponseToken,
                    "interaction response token does not match",
                ));
            }
            if interaction_route_epoch(&interaction.route) != exact.route_epoch {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleResponseRoute,
                    "interaction response route is stale",
                ));
            }
            if !interaction_route_admits_exact(
                &interaction.route,
                client.attachment_id(),
                exact.route_epoch,
                &exact.grant_token,
            ) {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::WrongAttachment,
                    "attachment does not hold the exact private response grant",
                ));
            }
        }
        if interaction.cancelled.is_some() {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::IllegalState,
                "interaction is already terminal",
            ));
        }
        if interaction.record.kind != expected_kind
            || interaction_answer_kind(response.answer()) != interaction.record.kind
        {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongInteractionKind,
                "interaction request and answer kinds do not match",
            ));
        }
        if interaction.record.answer_policy != *response.policy() {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::InvalidInput,
                "interaction answer policy does not match the persisted request",
            ));
        }
        if !interaction_answer_authority_matches(
            &interaction.record.request,
            response.answer(),
            response.authority(),
        ) {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongAuthorityFingerprint,
                "interaction response authority does not match the persisted request",
            ));
        }
        if let surface::SurfaceClientInteractionAnswer::PermissionRequest { decision } =
            response.answer()
            && decision.validate_scope().is_err()
        {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::InvalidInput,
                "denied permission responses must be turn-scoped",
            ));
        }
        let session_permissions = if let (
            surface::SurfaceInteractionRequest::PermissionRequest {
                permissions: requested,
                ..
            },
            surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision:
                    surface::SurfacePermissionClientDecision::Allow {
                        scope, permissions, ..
                    },
            },
        ) = (&interaction.record.request, response.answer())
        {
            if !surface_permission_profile_is_subset(permissions, requested) {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::InvalidInput,
                    "permission response exceeds the persisted requested profile",
                ));
            }
            (*scope == surface::PermissionGrantScope::Session).then(|| permissions.clone())
        } else {
            None
        };
        let session_permission_settings = match session_permissions.as_ref() {
            Some(permissions) => match self.surface_session_permission_settings(permissions) {
                Ok(settings) => settings,
                Err(message) => {
                    return Ok(Self::uncommitted_interaction_response(
                        request_id,
                        interaction,
                        surface::SurfaceMutationErrorCode::InvalidInput,
                        message,
                    ));
                }
            },
            None => None,
        };
        if !self
            .resident_surface
            .hub
            .admits_interaction_client(client, expected_kind)
            || !interaction_route_admits(&interaction.route, client.attachment_id())
        {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongAttachment,
                "attachment does not hold the current interaction response grant",
            ));
        }
        if !interaction_answer_within_private_limits(response.answer()) {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::InvalidInput,
                "interaction answer exceeds private retention limits",
            ));
        }
        if let Some(winning_receipt) = interaction.winning_receipt.clone() {
            let acknowledgement = interaction
                .resolution_ack
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![acknowledgement])
                        .expect("interaction replay has one acknowledgement"),
                },
                value: surface::RespondInteractionOutput {
                    interaction_id,
                    attempted_response_id: response.response_id().clone(),
                    disposition: surface::RespondInteractionDisposition::AlreadyResolved {
                        winning_receipt,
                    },
                    projected_cursor: interaction.projected_cursor.clone(),
                },
            });
        }
        let expected_revision = interaction.revision;
        let next_revision = surface::InteractionRevision::try_new(
            expected_revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let fence = interaction.record.fence.clone();
        let background_fence = (interaction.record.kind
            == surface::SurfaceInteractionKind::BackgroundApproval)
            .then(|| {
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .background_operations
                    .iter()
                    .find(|background| {
                        background.operation_id == fence.operation_id
                            && background.fence.operation_fence == fence
                    })
                    .map(|background| background.fence.clone())
            })
            .flatten();
        if interaction.record.kind == surface::SurfaceInteractionKind::BackgroundApproval
            && background_fence.is_none()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let attempted_digest = keyed_interaction_response_digest(
            &interaction.record.response_token,
            response.answer(),
        );
        let (receipt, winner_answer, attempted_private_winner) =
            match interaction.private_response.as_ref() {
                Some(winner) => (
                    winner.record.receipt.clone(),
                    winner.answer.clone(),
                    winner.record.receipt.response_id == *response.response_id()
                        && winner.record.keyed_response_digest == attempted_digest,
                ),
                None => {
                    let receipt = surface::SurfaceInteractionResolutionReceipt {
                        response_id: response.response_id().clone(),
                        receipt_id: surface::SurfaceResponseReceiptId::try_from_bytes(
                            *uuid::Uuid::now_v7().as_bytes(),
                        )
                        .expect("generated UUID is v7"),
                        kind: expected_kind,
                        safe_projection: interaction_safe_projection(response.answer()),
                    };
                    let private_response = ResidentPrivateInteractionResponse {
                        record: surface::BrokerInteractionResponseRecord {
                            receipt: receipt.clone(),
                            payload: surface::BrokerResponsePayload::LiveOnly {
                                incarnation: self
                                    .resident_surface
                                    .coordinator
                                    .state()
                                    .snapshot()
                                    .cursor
                                    .incarnation
                                    .clone(),
                            },
                            keyed_response_digest: attempted_digest,
                        },
                        answer: response.answer().clone(),
                        pending_batch: None,
                        retry_at: None,
                    };
                    self.resident_surface
                        .interactions
                        .get_mut(&interaction_id)
                        .expect("validated interaction remains resident")
                        .private_response = Some(private_response);
                    (receipt, response.answer().clone(), true)
                }
            };
        let winner_has_session_scope = matches!(
            &winner_answer,
            surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision: surface::SurfacePermissionClientDecision::Allow {
                    scope: surface::PermissionGrantScope::Session,
                    ..
                },
            }
        );
        // Child interactions always carry a task transition that clears the
        // pending approval in the same generation batch. Route those
        // resolutions through the owner-aware authority even for Turn-scoped
        // decisions (which do not have a Session settings patch).
        let child_permission_resolution = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .is_some_and(|interaction| {
                matches!(
                    &interaction.record.request,
                    surface::SurfaceInteractionRequest::PermissionRequest {
                        context: surface::SurfacePermissionContext {
                            owner: surface::SurfacePermissionOwnerRef::Child { .. },
                            ..
                        },
                        ..
                    }
                )
            });
        let detached_child_permission_resolution = child_permission_resolution
            && self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .interactions
                .iter()
                .find(|candidate| candidate.interaction_id == interaction_id)
                .and_then(|interaction| match &interaction.request {
                    surface::SurfaceInteractionRequest::PermissionRequest { context, .. } => {
                        match &context.owner {
                            surface::SurfacePermissionOwnerRef::Child { agent_id, .. } => self
                                .resident_surface
                                .coordinator
                                .state()
                                .snapshot()
                                .subagents
                                .iter()
                                .find(|subagent| subagent.subagent_id == *agent_id)
                                .map(|subagent| {
                                    matches!(
                                        &subagent.owner,
                                        surface::SurfaceSubagentOwner::DetachedTask { .. }
                                    )
                                }),
                            surface::SurfacePermissionOwnerRef::Foreground { .. } => None,
                        }
                    }
                    _ => None,
                })
                .unwrap_or(false);
        let batch = if let Some(batch) = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .and_then(|interaction| interaction.private_response.as_ref())
            .and_then(|private| private.pending_batch.clone())
        {
            batch
        } else {
            let scope = if detached_child_permission_resolution {
                surface::SurfaceScope::Thread
            } else {
                background_fence
                    .as_ref()
                    .map(|fence| surface::SurfaceScope::Background {
                        fence: fence.clone(),
                    })
                    .unwrap_or_else(|| surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    })
            };
            let continuation = self
                .resident_surface
                .interactions
                .continuation_turn_owners
                .get(&interaction_id)
                .map(|owner| owner.durable_answer(&receipt, &winner_answer))
                .transpose()
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
                .flatten();
            let mut events = Vec::new();
            if let Some((settings, _)) = session_permission_settings.as_ref() {
                let previous_revision = self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .settings
                    .thread_revision;
                events.push((
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Settings(surface::SettingsPatch::Committed {
                        previous_revision,
                        snapshot: settings.clone(),
                    }),
                ));
            }
            events.push((
                scope,
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Resolved {
                    interaction_id: interaction_id.clone(),
                    expected_revision,
                    next_revision,
                    receipt: receipt.clone(),
                    continuation,
                }),
            ));
            if let surface::SurfaceInteractionRequest::PermissionRequest { context, .. } =
                &interaction.record.request
                && let surface::SurfacePermissionOwnerRef::Child { task_id, .. } = &context.owner
            {
                let current_task = self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .tasks
                    .iter()
                    .find(|task| task.task_id == *task_id)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                if current_task.pending_interaction_id.as_ref() != Some(&interaction_id)
                    || current_task.status != surface::SurfaceTaskStatus::ApprovalRequired
                {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                let next_task_revision = surface::TaskRevision::try_new(
                    current_task
                        .revision
                        .get()
                        .checked_add(1)
                        .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                )
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                events.push((
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::InteractionChanged {
                        task_id: task_id.clone(),
                        expected_revision: current_task.revision,
                        next_revision: next_task_revision,
                        status: surface::SurfaceTaskStatus::Running,
                        pending_interaction_id: None,
                    }),
                ));
            }
            let batch = self.surface_event_batch_with_commit_id(events, None);
            self.resident_surface
                .interactions
                .get_mut(&interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
                .expect("private winner exists before public resolution")
                .pending_batch = Some(batch.clone());
            batch
        };
        let resolution = match background_fence {
            Some(background_fence) => {
                let safe_projection = interaction_safe_projection(&winner_answer);
                self.resident_surface
                    .coordinator
                    .commit_provider_background_interaction_resolution_batch(
                        background_fence,
                        &safe_projection,
                        &batch,
                    )
                    .map(|_| ())
                    .map_err(|_| ())
            }
            None if child_permission_resolution
                || (winner_has_session_scope && session_permission_settings.is_some()) =>
            {
                self.resident_surface
                    .coordinator
                    .commit_actor_generation_permission_resolution_batch(fence.clone(), &batch)
                    .map(|_| ())
                    .map_err(|error| {
                        eprintln!("orca: session permission atomic commit failed: {error:?}");
                        ()
                    })
            }
            None => self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence, &batch)
                .map(|_| ())
                .map_err(|_| ()),
        };
        if let Err(error) = resolution {
            eprintln!("orca: typed interaction resolution commit failed: {error:?}");
            self.resident_surface
                .interactions
                .get_mut(&interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
                .expect("failed private resolution remains resident")
                .retry_at =
                Some(tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if winner_has_session_scope {
            if let Some((_, config)) = session_permission_settings.as_ref() {
                self.config = config.clone();
            } else {
                let settings = self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .settings
                    .effective
                    .clone();
                hydrate_run_config_from_surface_settings(&mut self.config, &settings)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            }
        }
        self.apply_surface_interaction_resolution(&interaction_id, &winner_answer);
        let output = surface::RespondInteractionOutput {
            interaction_id: interaction_id.clone(),
            attempted_response_id: response.response_id().clone(),
            disposition: if attempted_private_winner {
                surface::RespondInteractionDisposition::Resolved { receipt }
            } else {
                surface::RespondInteractionDisposition::AlreadyResolved {
                    winning_receipt: receipt,
                }
            },
            projected_cursor: Some(batch.cursor_after.clone()),
        };
        Ok(Self::committed_interaction_mutation(
            request_id,
            interaction_id,
            &batch,
            output,
        ))
    }

    pub(super) fn apply_surface_interaction_resolution(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
        winner_answer: &surface::SurfaceClientInteractionAnswer,
    ) {
        let (record, waiter) = {
            let interaction = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .expect("committed interaction remains resident");
            // Keep a detached waiter resident until the mailbox response has
            // been durably written. Surface commits and task-registry commits
            // use separate ledgers; consuming the waiter here would otherwise
            // strand the child if the second write fails. The private response
            // itself is still cleared immediately because the durable receipt
            // below is sufficient for retry and avoids retaining client data.
            let retain_detached_waiter = matches!(
                interaction.waiter.as_ref(),
                Some(ResidentInteractionWaiter::DetachedPermission { .. })
            );
            let private = interaction
                .private_response
                .take()
                .expect("committed interaction retains its private winner");
            let batch = private
                .pending_batch
                .expect("committed interaction retains its exact public batch");
            let envelope = batch
                .events
                .as_slice()
                .iter()
                .find(|envelope| {
                    matches!(
                        &envelope.event,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Resolved { interaction_id: resolved, .. }
                        ) if *resolved == *interaction_id
                    )
                })
                .expect("committed interaction batch contains its resolution event");
            interaction.revision =
                surface::InteractionRevision::try_new(interaction.revision.get().saturating_add(1))
                    .expect("interaction revision did not exhaust");
            interaction.winning_receipt = Some(private.record.receipt);
            interaction.resolution_ack = Some(surface::MutationCommitAck::ThreadLocalCursor {
                cursor: batch.cursor_after.clone(),
                family: surface::SurfaceFactFamily::Interaction,
                event_id: envelope.event_id.clone(),
                commit_class: batch.commit_class.clone(),
            });
            interaction.projected_cursor = Some(batch.cursor_after);
            let waiter = if retain_detached_waiter {
                interaction.waiter.clone()
            } else {
                interaction.waiter.take()
            };
            (interaction.record.clone(), waiter)
        };
        if let (
            surface::SurfaceInteractionRequest::BackgroundApproval { task, tool, .. },
            surface::SurfaceClientInteractionAnswer::BackgroundApproval { decision },
        ) = (&record.request, winner_answer)
        {
            let mut pending = PendingBackgroundApprovalResolution {
                fence: record.fence.clone(),
                task: task.clone(),
                tool: tool.clone(),
                decision: *decision,
                pending_commit: None,
                retry_at: tokio::time::Instant::now(),
            };
            let operation_id = pending.fence.operation_id.clone();
            if let Err(error) = self.settle_background_approval_resolution(&mut pending) {
                eprintln!("orca: background approval settlement deferred: {error}");
                self.background_controller
                    .retain_approval_resolution(operation_id, pending);
            }
        }
        let mut detached_resolution_persisted = false;
        if let Some(waiter) = waiter {
            match (waiter, winner_answer) {
                (
                    ResidentInteractionWaiter::ToolApproval {
                        approval_id,
                        waiter,
                    },
                    surface::SurfaceClientInteractionAnswer::ToolApproval { decision },
                ) => {
                    let (decision, reason) = match decision {
                        surface::SurfaceAllowDeny::Allow => (
                            orca_core::approval_types::ApprovalDecision::Allow,
                            "approved through runtime surface",
                        ),
                        surface::SurfaceAllowDeny::Deny => (
                            orca_core::approval_types::ApprovalDecision::Deny,
                            "denied through runtime surface",
                        ),
                    };
                    let _ = waiter.send(Ok(orca_core::approval_types::ApprovalResolution {
                        id: approval_id,
                        decision,
                        reason: reason.to_string(),
                    }));
                }
                (
                    ResidentInteractionWaiter::Permission(waiter),
                    surface::SurfaceClientInteractionAnswer::PermissionRequest { decision },
                ) => {
                    let (decision, scope, permissions, strict_auto_review) = match decision {
                        surface::SurfacePermissionClientDecision::Allow {
                            scope,
                            permissions,
                            strict_auto_review,
                        } => (
                            crate::protocol::PermissionResponseDecision::Allow,
                            scope,
                            permissions,
                            strict_auto_review,
                        ),
                        surface::SurfacePermissionClientDecision::Deny {
                            permissions,
                            strict_auto_review,
                            ..
                        } => (
                            crate::protocol::PermissionResponseDecision::Deny,
                            &surface::PermissionGrantScope::Turn,
                            permissions,
                            strict_auto_review,
                        ),
                    };
                    let scope = match scope {
                        surface::PermissionGrantScope::Turn => {
                            crate::protocol::PermissionGrantScope::Turn
                        }
                        surface::PermissionGrantScope::Session => {
                            crate::protocol::PermissionGrantScope::Session
                        }
                    };
                    let _ = waiter.send(Ok(crate::runtime_permission::RuntimePermissionResponse {
                        decision,
                        scope,
                        permissions: runtime_permission_profile_from_surface(permissions),
                        strict_auto_review: *strict_auto_review,
                    }));
                }
                (
                    ResidentInteractionWaiter::DetachedPermission {
                        registry,
                        key,
                        request_digest,
                    },
                    surface::SurfaceClientInteractionAnswer::PermissionRequest { decision },
                ) => {
                    let (decision, scope, permissions, strict_auto_review) = match decision {
                        surface::SurfacePermissionClientDecision::Allow {
                            scope,
                            permissions,
                            strict_auto_review,
                        } => (
                            crate::protocol::PermissionResponseDecision::Allow,
                            scope,
                            permissions,
                            strict_auto_review,
                        ),
                        surface::SurfacePermissionClientDecision::Deny {
                            permissions,
                            strict_auto_review,
                            ..
                        } => (
                            crate::protocol::PermissionResponseDecision::Deny,
                            &surface::PermissionGrantScope::Turn,
                            permissions,
                            strict_auto_review,
                        ),
                    };
                    let response = crate::runtime_permission::RuntimePermissionResponse {
                        decision,
                        scope: match scope {
                            surface::PermissionGrantScope::Turn => {
                                crate::protocol::PermissionGrantScope::Turn
                            }
                            surface::PermissionGrantScope::Session => {
                                crate::protocol::PermissionGrantScope::Session
                            }
                        },
                        permissions: runtime_permission_profile_from_surface(permissions),
                        strict_auto_review: *strict_auto_review,
                    };
                    let expected_response = response.clone();
                    if let Err(error) = registry.resolve_detached_permission_request(
                        &key,
                        &request_digest,
                        response,
                    ) {
                        let terminal = match registry.detached_permission_request(&key) {
                            Ok(None) => true,
                            Ok(Some(record)) => {
                                record.response.as_ref() == Some(&expected_response)
                            }
                            Err(_) => false,
                        };
                        if terminal {
                            detached_resolution_persisted = true;
                        } else {
                            eprintln!(
                                "orca: failed to persist detached permission resolution; waiter retained for retry: {error}"
                            );
                        }
                    } else {
                        detached_resolution_persisted = true;
                    }
                }
                (
                    ResidentInteractionWaiter::UserInput(waiter),
                    surface::SurfaceClientInteractionAnswer::UserInput { decision },
                ) => {
                    let answer = match decision {
                        surface::SurfaceUserInputDecision::Answer(answer) => {
                            Some(answer.as_str().to_string())
                        }
                        surface::SurfaceUserInputDecision::Cancel => None,
                    };
                    let _ = waiter.send(Ok(answer));
                }
                (
                    ResidentInteractionWaiter::McpElicitation(waiter),
                    surface::SurfaceClientInteractionAnswer::McpElicitation { decision },
                ) => {
                    let response = match decision {
                        surface::SurfaceMcpElicitationDecision::Accept { content } => {
                            orca_mcp::McpElicitationResponse::Accept {
                                content: json_from_surface_data(content),
                            }
                        }
                        surface::SurfaceMcpElicitationDecision::Decline => {
                            orca_mcp::McpElicitationResponse::Decline
                        }
                    };
                    let _ = waiter.send(Ok(response));
                }
                _ => unreachable!("waiter and answer kind were validated before commit"),
            }
        }
        if detached_resolution_persisted
            && let Some(interaction) = self.resident_surface.interactions.get_mut(interaction_id)
        {
            interaction.waiter = None;
            interaction.private_response = None;
        }
        self.settle_cold_recovery_tool_approval(interaction_id, winner_answer);
        self.settle_cold_recovery_permission(interaction_id, winner_answer);
        self.settle_cold_recovery_continuation_turn(interaction_id);
    }

    /// Function intent contract:
    ///
    /// - Input: one durably resolved interaction owned by the cold-recovery
    ///   ToolApproval owner.
    /// - Output: an allow dispatches the persisted BeforeInvocation intent;
    ///   deny skips execution. Both paths release the historical operation by
    ///   completing its durable recovery finalization.
    /// - Errors: execution or finalization failures are retained as a blocked
    ///   surface condition after the interaction resolution remains committed.
    /// - State changes and external calls: allow may run hooks/router only
    ///   after durable InvocationStarted; deny performs no tool side effect.
    pub(super) fn settle_cold_recovery_tool_approval(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
        winner_answer: &surface::SurfaceClientInteractionAnswer,
    ) {
        let Some(owner) = self
            .resident_surface
            .interactions
            .cold_recovery_owners
            .get(interaction_id)
            .cloned()
        else {
            return;
        };
        let Some(receipt) = self
            .resident_surface
            .interactions
            .get(interaction_id)
            .and_then(|interaction| interaction.winning_receipt.clone())
        else {
            self.operation_recovery.terminal_blocked =
                Some("cold-recovery ToolApproval lost its durable resolution receipt".to_string());
            return;
        };
        let dispatch = match winner_answer {
            surface::SurfaceClientInteractionAnswer::ToolApproval {
                decision: surface::SurfaceAllowDeny::Allow,
            } => self
                .dispatch_cold_recovery_tool_approval(&owner, &receipt)
                .map(|_| ()),
            surface::SurfaceClientInteractionAnswer::ToolApproval {
                decision: surface::SurfaceAllowDeny::Deny,
            } => Ok(()),
            _ => return,
        };
        self.resident_surface
            .interactions
            .cold_recovery_owners
            .remove(interaction_id);
        if let Err(error) = dispatch {
            eprintln!("orca: recovered ToolApproval dispatch failed closed: {error}");
        }
        if let Err(error) = self.terminalize_cold_recovery_tool_approval(&owner) {
            self.operation_recovery.terminal_blocked = Some(format!(
                "cold-recovery ToolApproval operation terminalization failed: {error}"
            ));
        }
    }

    /// Function intent contract:
    ///
    /// - Input: one durably resolved PermissionRequest retained by a validated
    ///   cold-recovery pre-side-effect owner.
    /// - Output: allow re-dispatches the exact tool with the durable permission
    ///   answer; deny terminalizes without dispatch.
    /// - Errors: dispatch/finalization failures remain fail closed after the
    ///   interaction resolution is durable.
    /// - State changes and external calls: allow may execute the tool only
    ///   after durable `InvocationStarted`; deny performs no tool side effect.
    pub(super) fn settle_cold_recovery_permission(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
        winner_answer: &surface::SurfaceClientInteractionAnswer,
    ) {
        let Some(owner) = self
            .resident_surface
            .interactions
            .cold_recovery_permission_owners
            .get(interaction_id)
            .cloned()
        else {
            return;
        };
        let Some(receipt) = self
            .resident_surface
            .interactions
            .get(interaction_id)
            .and_then(|interaction| interaction.winning_receipt.clone())
        else {
            self.operation_recovery.terminal_blocked = Some(
                "cold-recovery PermissionRequest lost its durable resolution receipt".to_string(),
            );
            return;
        };
        let dispatch = match winner_answer {
            surface::SurfaceClientInteractionAnswer::PermissionRequest { decision } => {
                let (decision, scope, permissions, strict_auto_review) = match decision {
                    surface::SurfacePermissionClientDecision::Allow {
                        scope,
                        permissions,
                        strict_auto_review,
                    } => (
                        crate::protocol::PermissionResponseDecision::Allow,
                        *scope,
                        permissions,
                        *strict_auto_review,
                    ),
                    surface::SurfacePermissionClientDecision::Deny {
                        permissions,
                        strict_auto_review,
                        ..
                    } => (
                        crate::protocol::PermissionResponseDecision::Deny,
                        surface::PermissionGrantScope::Turn,
                        permissions,
                        *strict_auto_review,
                    ),
                };
                let response = crate::runtime_permission::RuntimePermissionResponse {
                    decision,
                    scope: match scope {
                        surface::PermissionGrantScope::Turn => {
                            crate::protocol::PermissionGrantScope::Turn
                        }
                        surface::PermissionGrantScope::Session => {
                            crate::protocol::PermissionGrantScope::Session
                        }
                    },
                    permissions: runtime_permission_profile_from_surface(permissions),
                    strict_auto_review,
                };
                if decision == crate::protocol::PermissionResponseDecision::Allow {
                    self.dispatch_cold_recovery_permission(&owner, &receipt, &response)
                        .map(|_| ())
                } else {
                    Ok(())
                }
            }
            _ => return,
        };
        self.resident_surface
            .interactions
            .cold_recovery_permission_owners
            .remove(interaction_id);
        if let Err(error) = dispatch {
            eprintln!("orca: recovered PermissionRequest dispatch failed closed: {error}");
        }
        if let Err(error) = self.terminalize_cold_recovery_permission(&owner) {
            self.operation_recovery.terminal_blocked = Some(format!(
                "cold-recovery PermissionRequest operation terminalization failed: {error}"
            ));
        }
    }

    /// Function intent contract:
    ///
    /// - Input: one durably resolved recovered UserInput/MCP interaction.
    /// - Output: cancel/decline only terminalizes the historical operation;
    ///   an accepted answer atomically creates one stable durable operation
    ///   intent and then executes that same operation/turn identity.
    /// - Errors: invalid capsules, stale owners, terminalization failures, or
    ///   turn-start failures remain fail closed after the resolution commit.
    /// - State changes: `Started` means the stable durable operation exists,
    ///   not that a process-local `StartTurn` call happened. Recovery retries
    ///   until the stable turn is durably present; `Consumed` is written only
    ///   after that durable turn boundary or operation terminal is observable.
    pub(super) fn settle_cold_recovery_continuation_turn(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) {
        let Some(owner) = self
            .resident_surface
            .interactions
            .continuation_turn_owners
            .get(interaction_id)
            .cloned()
        else {
            return;
        };
        let Some(receipt) = self
            .resident_surface
            .interactions
            .get(interaction_id)
            .and_then(|interaction| interaction.winning_receipt.clone())
        else {
            let terminalization =
                self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
            self.operation_recovery.terminal_blocked = Some(match terminalization {
                Ok(()) => {
                    "cold-recovery continuation lost its durable resolution receipt".to_string()
                }
                Err(error) => format!(
                    "cold-recovery continuation lost its durable resolution receipt and terminalization failed: {error}"
                ),
            });
            return;
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let owner_valid = snapshot.thread.owner_epoch == owner.cold_owner_epoch
            && snapshot.interactions.iter().any(|interaction| {
                interaction.interaction_id == owner.interaction_id
                    && interaction.fence == owner.historical_fence
                    && matches!(
                        interaction.lifecycle,
                        surface::SurfaceInteractionLifecycle::Resolved {
                            receipt: ref durable_receipt,
                        } if durable_receipt == &receipt
                    )
            });
        let recovered = self
            .resident_surface
            .coordinator
            .ledger()
            .recover_batches()
            .map(|batches| {
                recovered_continuation_resolutions_from_batches(&batches.committed)
                    .remove(interaction_id)
            });
        if !continuation_resolution_requires_dispatch(&receipt) {
            self.resident_surface
                .interactions
                .continuation_turn_owners
                .remove(interaction_id);
            if let Err(error) =
                self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation")
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cold-recovery continuation cancellation terminalization failed: {error}"
                ));
            }
            return;
        }
        let resolution = match recovered {
            Ok(Some(resolution)) if owner_valid && resolution.receipt == receipt => resolution,
            Ok(_) => {
                self.resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(interaction_id);
                let _ =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
                self.operation_recovery.terminal_blocked = Some(
                    "cold-recovery continuation lacks a valid durable answer fact".to_string(),
                );
                return;
            }
            Err(error) => {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cold-recovery continuation fact recovery failed: {error:?}"
                ));
                return;
            }
        };
        let identity = match owner.continuation_operation_identity(&receipt) {
            Ok(identity) => identity,
            Err(error) => {
                self.resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(interaction_id);
                let _ =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cold-recovery continuation identity is invalid: {error}"
                ));
                return;
            }
        };
        let operation_id = identity.operation_id().clone();
        let turn_id = identity.turn_id().clone();
        let dispatch_id = identity.dispatch_id().clone();
        match &resolution.dispatch_state {
            RecoveredContinuationDispatchState::Pending => {}
            RecoveredContinuationDispatchState::Started {
                dispatch_id: recorded_dispatch,
                operation_id: recorded_operation,
                turn_id: recorded_turn,
            }
            | RecoveredContinuationDispatchState::Consumed {
                dispatch_id: recorded_dispatch,
                operation_id: recorded_operation,
                turn_id: recorded_turn,
            } if recorded_dispatch != &dispatch_id
                || recorded_operation != &operation_id
                || recorded_turn != &turn_id =>
            {
                self.resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(interaction_id);
                let _ =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
                self.operation_recovery.terminal_blocked = Some(
                    "cold-recovery continuation dispatch identity is inconsistent".to_string(),
                );
                return;
            }
            RecoveredContinuationDispatchState::Consumed { .. } => {
                self.resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(interaction_id);
                if let Err(error) =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation")
                {
                    self.operation_recovery.terminal_blocked = Some(format!(
                        "consumed continuation terminalization failed: {error}"
                    ));
                }
                return;
            }
            RecoveredContinuationDispatchState::Started { .. } => {}
        }
        let Some(answer) = resolution.answer else {
            self.resident_surface
                .interactions
                .continuation_turn_owners
                .remove(interaction_id);
            let _ = self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
            self.operation_recovery.terminal_blocked =
                Some("accepted continuation resolution has no durable answer".to_string());
            return;
        };
        let notification = match owner.render_pinned_user_continuation(&receipt, &answer) {
            Ok(notification) => notification,
            Err(error) => {
                self.resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(interaction_id);
                let _ =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation");
                self.operation_recovery.terminal_blocked =
                    Some(format!("recovered continuation failed closed: {error}"));
                return;
            }
        };
        if matches!(
            resolution.dispatch_state,
            RecoveredContinuationDispatchState::Pending
        ) && let Err(error) =
            self.commit_cold_recovery_continuation_operation_intent(&owner, &receipt, &identity)
        {
            self.operation_recovery.terminal_blocked = Some(format!(
                "cold-recovery continuation operation intent commit failed: {error}"
            ));
            return;
        }
        if let Err(error) =
            self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation")
        {
            self.operation_recovery.terminal_blocked = Some(format!(
                "cold-recovery continuation historical operation terminalization failed: {error}"
            ));
            return;
        }
        let durable_turn_exists = self.has_durable_continuation_turn(&turn_id);
        let continuation_terminal = Self::surface_operation_record(
            self.resident_surface.coordinator.state().snapshot(),
            &operation_id,
        )
        .is_some_and(|operation| operation.terminal.is_some());
        if durable_turn_exists || continuation_terminal {
            if durable_turn_exists
                && !continuation_terminal
                && let Err(error) =
                    self.terminalize_cold_recovery_operation(&operation_id, "continuation turn")
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cold-recovery continuation durable turn terminalization failed: {error}"
                ));
                return;
            }
            if let Err(error) = self.commit_cold_recovery_continuation_dispatch_consumed(
                &owner,
                &receipt,
                dispatch_id,
                operation_id,
                turn_id,
            ) {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cold-recovery continuation consumption commit failed: {error}"
                ));
                return;
            }
            self.resident_surface
                .interactions
                .continuation_turn_owners
                .remove(interaction_id);
            return;
        }
        if let Err(error) = self.start_cold_recovery_continuation_operation(
            &owner,
            &receipt,
            notification,
            operation_id,
            turn_id,
        ) {
            self.operation_recovery.terminal_blocked = Some(format!(
                "cold-recovery continuation turn start failed: {error}"
            ));
        }
    }

    pub(super) fn has_durable_continuation_turn(&self, turn_id: &TurnId) -> bool {
        self.state
            .as_ref()
            .and_then(|state| state.thread.session().conversation_records())
            .is_some_and(|records| {
                records
                    .iter()
                    .any(|record| record.turn_id.as_ref() == Some(turn_id))
            })
    }

    pub(super) fn commit_cold_recovery_continuation_operation_intent(
        &mut self,
        owner: &ContinuationTurnCheckpointOwner,
        receipt: &surface::SurfaceInteractionResolutionReceipt,
        identity: &surface::DurableInteractionContinuationOperationIdentity,
    ) -> io::Result<()> {
        let interaction = self
            .resident_surface
            .interactions
            .get(&owner.interaction_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "interaction missing"))?;
        let expected_revision = interaction.revision;
        let next_revision = surface::InteractionRevision::try_new(
            expected_revision
                .get()
                .checked_add(1)
                .ok_or_else(|| io::Error::other("interaction revision exhausted"))?,
        )
        .map_err(|_| io::Error::other("interaction revision is invalid"))?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let historical_operation = Self::surface_operation_record(&snapshot, owner.operation_id())
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "historical continuation operation is missing",
                )
            })?;
        let origin = historical_operation.intent.origin;
        let origin_attachment = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(owner.operation_id())
            .cloned();
        if matches!(&origin, surface::OperationOrigin::AcpPrompt { .. })
            && !origin_attachment
                .as_ref()
                .is_some_and(|attachment| self.resident_surface.hub.has_live_attachment(attachment))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "ACP continuation origin attachment is unavailable",
            ));
        }
        let reference = surface::SurfaceInputRequest {
            blocks: surface::NonEmptyVec::try_new(vec![surface::SurfaceInputRequestBlock::Text {
                text: surface::DisplayText::new(format!(
                    "durable continuation reference: interaction={} receipt={}",
                    uuid::Uuid::from_bytes(*owner.interaction_id.as_bytes()),
                    uuid::Uuid::from_bytes(*receipt.receipt_id.as_bytes()),
                )),
            }])
            .expect("continuation reference input is non-empty"),
        };
        let request_digest = surface_sha256(
            &serde_json::to_vec(&reference).expect("continuation reference is serializable"),
        );
        if let Some(operation) = Self::surface_operation_record(
            self.resident_surface.coordinator.state().snapshot(),
            identity.operation_id(),
        ) {
            let exact_replayability = matches!(
                &operation.intent.initial_replayability,
                surface::Replayability::Replayable {
                    request: Some(request),
                    request_digest: Some(digest),
                    ..
                } if request == &reference && digest == &request_digest
            );
            if operation.request_id == *identity.request_id()
                && operation.intent.kind == surface::OperationKind::UserTurn
                && exact_replayability
            {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stable continuation operation identity is occupied by different work",
            ));
        }
        let replayability = surface::Replayability::Replayable {
            capsule_digest: request_digest.clone(),
            request: Some(reference),
            request_digest: Some(request_digest),
            cwd: snapshot.settings.effective.cwd.clone(),
            workspace_roots: snapshot.settings.effective.workspace_roots.clone(),
            settings_revision: snapshot.settings.thread_revision,
            policy_epoch: snapshot.settings.effective.policy_epoch,
            tool_schema_digest: surface_sha256(
                &serde_json::to_vec(&snapshot.tools).expect("surface tools are serializable"),
            ),
        };
        let capability_fingerprint = crate::runtime_host::surface_capability_fingerprint(
            &snapshot.settings.effective,
            &snapshot.tools,
        );
        let operation_id = identity.operation_id().clone();
        let lease = surface::ReservationLease::new(
            surface::SurfaceAdmissionLeaseId::try_from_bytes(*owner.interaction_id.as_bytes())
                .expect("interaction identity is a UUIDv7"),
            operation_id.clone(),
            surface::SequenceNumber::new(snapshot.queued_operations.len() as u64 + 1),
            self.resident_surface
                .hub
                .authority()
                .host_incarnation()
                .clone(),
            surface::MonotonicInstant {
                clock_id: surface::HostMonotonicClockId::try_from_bytes(
                    *receipt.response_id.as_bytes(),
                )
                .expect("response identity is a UUIDv7"),
                tick: surface::MonotonicTick::new(0),
            },
        );
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: identity.request_id().clone(),
            intent: surface::OperationIntent {
                origin,
                kind: surface::OperationKind::UserTurn,
                initial_replayability: replayability,
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint,
                settings_receipt: surface::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: surface::OperationPhase::Requested,
            reservation: lease,
            ready_for_admission: true,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let patch = surface::InteractionPatch::ContinuationDispatchStarted {
            interaction_id: owner.interaction_id.clone(),
            expected_revision,
            next_revision,
            receipt_id: receipt.receipt_id.clone(),
            dispatch_id: identity.dispatch_id().clone(),
            operation_id: operation_id.clone(),
            turn_id: identity.turn_id().clone(),
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Requested {
                        operation,
                    }),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Interaction(patch),
                ),
            ],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|error| {
                io::Error::other(format!("continuation operation commit failed: {error:?}"))
            })?;
        self.resident_surface
            .interactions
            .get_mut(&owner.interaction_id)
            .expect("committed continuation interaction remains resident")
            .revision = next_revision;
        if let Some(origin_attachment) = origin_attachment {
            self.resident_surface
                .interactions
                .operation_origin_attachments
                .insert(operation_id, origin_attachment);
        }
        Ok(())
    }

    pub(super) fn commit_cold_recovery_continuation_dispatch_consumed(
        &mut self,
        owner: &ContinuationTurnCheckpointOwner,
        receipt: &surface::SurfaceInteractionResolutionReceipt,
        dispatch_id: surface::SurfaceSettlementId,
        operation_id: surface::SurfaceOperationId,
        turn_id: TurnId,
    ) -> io::Result<()> {
        let interaction = self
            .resident_surface
            .interactions
            .get(&owner.interaction_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "interaction missing"))?;
        let expected_revision = interaction.revision;
        let next_revision = surface::InteractionRevision::try_new(
            expected_revision
                .get()
                .checked_add(1)
                .ok_or_else(|| io::Error::other("interaction revision exhausted"))?,
        )
        .map_err(|_| io::Error::other("interaction revision is invalid"))?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Interaction(
                    surface::InteractionPatch::ContinuationDispatchConsumed {
                        interaction_id: owner.interaction_id.clone(),
                        expected_revision,
                        next_revision,
                        receipt_id: receipt.receipt_id.clone(),
                        dispatch_id,
                        operation_id,
                        turn_id,
                    },
                ),
            )],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|error| {
                io::Error::other(format!("continuation consumption commit failed: {error:?}"))
            })?;
        self.resident_surface
            .interactions
            .get_mut(&owner.interaction_id)
            .expect("committed continuation interaction remains resident")
            .revision = next_revision;
        Ok(())
    }

    pub(super) fn prepare_cold_recovery_continuation_generation(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
        turn_id: &TurnId,
    ) -> io::Result<surface::SurfaceOperationFence> {
        loop {
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let operation = Self::surface_operation_record(&snapshot, operation_id)
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "continuation operation intent is missing",
                    )
                })?;
            match operation.phase {
                surface::OperationPhase::Requested => {
                    if snapshot.foreground_operation.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "continuation operation cannot admit beside a foreground operation",
                        ));
                    }
                    let fence = surface::SurfaceOperationFence {
                        thread_id: snapshot.thread.thread_id.clone(),
                        thread_owner_epoch: snapshot.thread.owner_epoch,
                        operation_id: operation_id.clone(),
                        generation_id: surface::SurfaceGenerationId::new(0),
                    };
                    let generation = surface::GenerationRecord {
                        fence: fence.clone(),
                        logical_turn_id: turn_id.clone(),
                        input: surface::GenerationInputState::NotApplicable,
                        predecessor: None,
                        attempt: surface::GenerationAttempt::Initial,
                        goal_identity: None,
                        replayability: operation.intent.initial_replayability.clone(),
                        required_capabilities: operation.intent.required_capabilities.clone(),
                        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
                        phase: surface::GenerationPhase::Reserved,
                        started_witness: None,
                        stop_reason: None,
                    };
                    let batch = self.surface_operation_batch(
                        operation_id,
                        vec![surface::OperationPatch::Admitted {
                            operation_id: operation_id.clone(),
                            logical_turn_id: turn_id.clone(),
                            input: surface::AdmittedInput::NotApplicable,
                            first_generation: generation,
                        }],
                    );
                    self.resident_surface
                        .coordinator
                        .commit_actor_batch(&batch)
                        .map_err(|error| {
                            io::Error::other(format!(
                                "continuation operation admission failed: {error:?}"
                            ))
                        })?;
                }
                surface::OperationPhase::Admitted => {
                    let generation = operation.generations.last().cloned().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "admitted continuation operation has no generation",
                        )
                    })?;
                    if generation.logical_turn_id != *turn_id {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "continuation operation turn identity changed",
                        ));
                    }
                    if generation.fence.thread_owner_epoch != snapshot.thread.owner_epoch {
                        let stop_reason = match generation.phase {
                            surface::GenerationPhase::Reserved => {
                                surface::GenerationStopReason::NotStarted {
                                    reason: surface::NotStartedReason::RuntimeRestart,
                                }
                            }
                            surface::GenerationPhase::Started
                            | surface::GenerationPhase::Transferred => {
                                surface::GenerationStopReason::RuntimeRestart
                            }
                            surface::GenerationPhase::Stopped => {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "stopped continuation generation is not suspended",
                                ));
                            }
                        };
                        let batch = self.surface_event_batch_with_commit_id(
                            vec![
                                (
                                    surface::SurfaceScope::Generation {
                                        fence: generation.fence.clone(),
                                    },
                                    surface::SurfaceEvent::Operation(
                                        surface::OperationPatch::GenerationStopped {
                                            fence: generation.fence.clone(),
                                            reason: stop_reason,
                                            usage_delta: surface::UsageTotals {
                                                input_tokens: 0,
                                                output_tokens: 0,
                                                cache_tokens: 0,
                                                estimated_cost_usd_micros: 0,
                                            },
                                        },
                                    ),
                                ),
                                (
                                    surface::SurfaceScope::Operation {
                                        operation_id: operation_id.clone(),
                                    },
                                    surface::SurfaceEvent::Operation(
                                        surface::OperationPatch::Suspended {
                                            operation_id: operation_id.clone(),
                                            cause: surface::SuspensionCause::RecoveryRequired {
                                                generation_id: generation.fence.generation_id,
                                            },
                                        },
                                    ),
                                ),
                            ],
                            None,
                        );
                        self.resident_surface
                            .coordinator
                            .commit_resume_abort_batch(generation.fence.clone(), &batch)
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "continuation generation recovery failed: {error:?}"
                                ))
                            })?;
                        continue;
                    }
                    if generation.phase == surface::GenerationPhase::Reserved {
                        let started_commit_id = surface::SurfaceCommitId::try_from_bytes(
                            *uuid::Uuid::now_v7().as_bytes(),
                        )
                        .expect("generated UUID is v7");
                        let batch = self.surface_operation_batch_with_commit_id(
                            operation_id,
                            vec![surface::OperationPatch::GenerationStarted {
                                fence: generation.fence.clone(),
                                witness: surface::GenerationStartedWitness {
                                    started_commit_id: started_commit_id.clone(),
                                    settings_revision: operation.intent.settings_revision,
                                    policy_epoch: operation.intent.policy_epoch,
                                    durable_replayability_digest:
                                        surface::canonical_replayability_digest(
                                            &generation.replayability,
                                        ),
                                    capability_fingerprint: generation
                                        .capability_fingerprint
                                        .clone(),
                                },
                            }],
                            Some(started_commit_id),
                        );
                        self.resident_surface
                            .coordinator
                            .commit_generation_batch(generation.fence.clone(), &batch)
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "continuation generation start failed: {error:?}"
                                ))
                            })?;
                        continue;
                    }
                    if generation.phase != surface::GenerationPhase::Started {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "continuation generation is not executable",
                        ));
                    }
                    if !operation
                        .agent_loop_turns
                        .iter()
                        .any(|turn| turn.fence == generation.fence && turn.turn_id == *turn_id)
                    {
                        let task_id = surface::SurfaceTaskId::try_new(format!(
                            "continuation-{}-{}",
                            uuid::Uuid::from_bytes(*operation_id.as_bytes()),
                            generation.fence.generation_id.get(),
                        ))
                        .expect("continuation task identity is non-empty");
                        let batch = self.surface_operation_batch(
                            operation_id,
                            vec![surface::OperationPatch::AgentLoopTurnStarted {
                                turn: surface::SurfaceAgentLoopTurn {
                                    turn_id: turn_id.clone(),
                                    fence: generation.fence.clone(),
                                    ordinal: 0,
                                    task_id,
                                    task_status: surface::SurfaceTaskRunningStatus::Running,
                                },
                            }],
                        );
                        self.resident_surface
                            .coordinator
                            .commit_generation_batch(generation.fence.clone(), &batch)
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "continuation agent-loop admission failed: {error:?}"
                                ))
                            })?;
                    }
                    return Ok(generation.fence);
                }
                surface::OperationPhase::Suspended {
                    cause: surface::SuspensionCause::RecoveryRequired { .. },
                } => {
                    let previous = operation.generations.last().cloned().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "suspended continuation operation has no generation",
                        )
                    })?;
                    if previous.phase != surface::GenerationPhase::Stopped {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "suspended continuation generation is not stopped",
                        ));
                    }
                    let generation_id = surface::SurfaceGenerationId::new(
                        previous
                            .fence
                            .generation_id
                            .get()
                            .checked_add(1)
                            .ok_or_else(|| {
                                io::Error::other("continuation generation identity exhausted")
                            })?,
                    );
                    let fence = surface::SurfaceOperationFence {
                        thread_id: snapshot.thread.thread_id.clone(),
                        thread_owner_epoch: snapshot.thread.owner_epoch,
                        operation_id: operation_id.clone(),
                        generation_id,
                    };
                    let generation = surface::GenerationRecord {
                        fence: fence.clone(),
                        logical_turn_id: turn_id.clone(),
                        input: surface::GenerationInputState::NotApplicable,
                        predecessor: Some(previous.fence),
                        attempt: surface::GenerationAttempt::RecoveryReplacement,
                        goal_identity: None,
                        replayability: previous.replayability,
                        required_capabilities: previous.required_capabilities,
                        capability_fingerprint: previous.capability_fingerprint,
                        phase: surface::GenerationPhase::Reserved,
                        started_witness: None,
                        stop_reason: None,
                    };
                    let batch = self.surface_operation_batch(
                        operation_id,
                        vec![
                            surface::OperationPatch::GenerationReserved {
                                generation: generation.clone(),
                            },
                            surface::OperationPatch::ControlIntentCommitted {
                                operation_id: operation_id.clone(),
                                request_id: operation.request_id,
                                intent: surface::PendingControlIntent::ResumeStarting {
                                    generation_fence: fence,
                                },
                            },
                        ],
                    );
                    self.resident_surface
                        .coordinator
                        .commit_actor_batch(&batch)
                        .map_err(|error| {
                            io::Error::other(format!(
                                "continuation recovery reservation failed: {error:?}"
                            ))
                        })?;
                }
                surface::OperationPhase::Terminal => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "continuation operation is terminal",
                    ));
                }
                surface::OperationPhase::Suspended { .. }
                | surface::OperationPhase::Finalizing { .. }
                | surface::OperationPhase::FinalizingDegraded { .. } => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "continuation operation is not recoverably executable",
                    ));
                }
            }
        }
    }

    /// Start only the durable operation/turn identity created by the
    /// interaction settlement. The pinned-user prompt is persisted under
    /// that stable turn before model execution; recovery uses that transcript
    /// fact as the at-most-once execution boundary.
    pub(super) fn start_cold_recovery_continuation_operation(
        &mut self,
        owner: &ContinuationTurnCheckpointOwner,
        receipt: &surface::SurfaceInteractionResolutionReceipt,
        notification: String,
        operation_id: surface::SurfaceOperationId,
        turn_id: TurnId,
    ) -> io::Result<()> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cold-recovery continuation cannot start beside a live operation",
            ));
        }
        if self.state.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "cold-recovery continuation thread state is unavailable",
            ));
        }
        let fence = self.prepare_cold_recovery_continuation_generation(&operation_id, &turn_id)?;
        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = fence.clone();
        let headless = Self::surface_operation_record(
            self.resident_surface.coordinator.state().snapshot(),
            &operation_id,
        )
        .is_some_and(|operation| {
            matches!(&operation.intent.origin, surface::OperationOrigin::Headless)
        });
        let request = if headless {
            HostedTurnRequest::headless_session(notification)
        } else {
            HostedTurnRequest::new(notification)
        }
        .with_turn_id(turn_id)
        .with_generation_handlers(move |_, cancel| {
            HostedGenerationHandlers::default()
                .with_provider_response_ingress(Arc::new(RuntimeSurfaceProviderResponseIngress {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_workflow_lifecycle_ingress(Arc::new(RuntimeSurfaceWorkflowLifecycleIngress {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_acp_read_text_file_handler(Arc::new(RuntimeSurfaceReadTextFileHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_acp_write_text_file_handler(Arc::new(RuntimeSurfaceWriteTextFileHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_acp_terminal_create_handler(Arc::new(RuntimeSurfaceTerminalCreateHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_approval_handler(Arc::new(RuntimeSurfaceApprovalHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                    cancel: cancel.clone(),
                }))
                .with_permission_handler(Arc::new(RuntimeSurfacePermissionHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                    cancel: cancel.clone(),
                }))
                .with_user_input_handler(Arc::new(RuntimeSurfaceUserInputHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                    cancel: cancel.clone(),
                }))
                .with_mcp_elicitation_handler(Arc::new(RuntimeSurfaceMcpElicitationHandler {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                    cancel,
                }))
        });
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(request),
            writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
            config: None,
            reply: start_tx,
        });
        start_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "continuation start closed"))?
            .map_err(|error| io::Error::other(error.to_string()))?;
        let active = self.active.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cold-recovery continuation did not become active",
            )
        })?;
        active.surface_operation = Some(fence);
        debug_assert_eq!(
            owner
                .continuation_operation_identity(receipt)
                .expect("validated continuation identity")
                .operation_id(),
            &operation_id
        );
        Ok(())
    }

    /// Resume durable resolved continuation work whenever the actor is idle.
    /// Requested interactions remain parked for a client answer; resolved
    /// Pending answers create their stable operation once; Started answers
    /// remain retryable until the stable turn record or operation terminal is
    /// durable. Consumed and negative receipts never launch another turn.
    pub(super) fn resume_recovered_continuation_turns(&mut self) {
        if self.resident_surface.0.is_none() {
            return;
        }
        loop {
            if self.active.is_some() || self.operation_recovery.terminal_blocked.is_some() {
                return;
            }
            let next = self
                .resident_surface
                .interactions
                .continuation_turn_owners
                .keys()
                .find(|interaction_id| {
                    self.resident_surface
                        .coordinator
                        .state()
                        .snapshot()
                        .interactions
                        .iter()
                        .any(|interaction| {
                            interaction.interaction_id == **interaction_id
                                && matches!(
                                    interaction.lifecycle,
                                    surface::SurfaceInteractionLifecycle::Resolved { .. }
                                )
                        })
                })
                .cloned();
            let Some(interaction_id) = next else {
                return;
            };
            let before = self
                .resident_surface
                .interactions
                .continuation_turn_owners
                .len();
            self.settle_cold_recovery_continuation_turn(&interaction_id);
            if self.active.is_some()
                || self
                    .resident_surface
                    .interactions
                    .continuation_turn_owners
                    .len()
                    >= before
            {
                return;
            }
        }
    }

    /// Function intent contract:
    ///
    /// - Input: a validated cold owner and its durable allow receipt.
    /// - Output: rebuilds thread-owned execution dependencies and invokes the
    ///   recovered dispatcher without recreating the operation or waiter.
    /// - Errors: rejects concurrent live execution or missing thread state;
    ///   downstream fingerprint/start/result errors remain fail closed.
    /// - State changes and external calls: hooks/router may run only through
    ///   `dispatch_after_approval`, after durable InvocationStarted authority.
    pub(super) fn dispatch_cold_recovery_tool_approval(
        &mut self,
        owner: &ColdRecoveryToolApprovalOwner,
        receipt: &surface::SurfaceInteractionResolutionReceipt,
    ) -> io::Result<ToolExecutionCompletion> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cold-recovery ToolApproval cannot dispatch beside a live operation",
            ));
        }
        let config = self.config.clone();
        let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
        let policy = crate::tool_execution::policy_for_tool_execution(&config);
        let cancel = CancelToken::new();
        let mut permission_overlay = crate::runtime_permission::TurnPermissionOverlay::default();
        let mut background_workflows = Vec::new();
        let mut state = self.state.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cold-recovery ToolApproval thread state is unavailable",
            )
        })?;
        let result = (|| {
            let thread_extensions = state.thread.thread_extensions_handle();
            let turn_extensions = crate::extension::ExtensionData::new(format!(
                "{}:recovered-tool-approval",
                state.thread.thread_id()
            ));
            let extension_registry = crate::extension::empty_extension_registry();
            let goal_runtime_binding = state
                .thread
                .thread_extensions()
                .get::<crate::goal_actor::GoalRuntimeBinding>()
                .map(|binding| (*binding).clone());
            let goal_mode = Self::surface_operation_record(
                self.resident_surface.coordinator.state().snapshot(),
                owner.operation_id(),
            )
            .is_some_and(|operation| {
                matches!(
                    operation.intent.kind,
                    surface::OperationKind::GoalRun { .. }
                )
            });
            let mut sink = EventSink::new(io::sink(), config.output_format);
            let parts = state.thread.session_mut().runtime_parts();
            owner.dispatch_after_approval(
                self,
                &config,
                &mut state.events,
                &mut sink,
                receipt,
                RecoveredToolExecutionDependencies {
                    cwd: &cwd,
                    subagent_depth: 0,
                    goal_mode,
                    policy: &policy,
                    instructions: parts.instructions,
                    memory: parts.memory,
                    mcp_registry: parts.mcp_registry,
                    hooks: parts.hooks,
                    cost_tracker: parts.cost_tracker,
                    cancel: &cancel,
                    task_registry: parts.task_registry,
                    background_workflows: &mut background_workflows,
                    workflow_ipc: None,
                    permission_overlay: &mut permission_overlay,
                    permission_handler: None,
                    user_input_handler: None,
                    mcp_elicitation_handler: None,
                    workflow_lifecycle_ingress: None,
                    wait_for_background_workflows: false,
                    extension_registry: Some(extension_registry.as_ref()),
                    extension_stores: Some(crate::extension::RuntimeExtensionStores::new(
                        thread_extensions.as_ref(),
                        &turn_extensions,
                    )),
                    goal_runtime_binding,
                    root_task_id: None,
                    child_budget: None,
                },
                crate::agent_loop::execute_child_agent_loop::<io::Sink>,
                crate::agent_loop::execute_child_agent_loop::<
                    crate::workflow::runner::SharedEventBuffer,
                >,
            )
        })();
        self.state = Some(state);
        result
    }

    /// Rebuild the original permission-retry execution context from its
    /// durable capsule. Only the existing bash sandbox/network retry path can
    /// create this owner; ordinary tool-internal permission requests cannot.
    pub(super) fn dispatch_cold_recovery_permission(
        &mut self,
        owner: &ColdRecoveryPermissionOwner,
        receipt: &surface::SurfaceInteractionResolutionReceipt,
        response: &crate::runtime_permission::RuntimePermissionResponse,
    ) -> io::Result<ToolExecutionCompletion> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cold-recovery PermissionRequest cannot dispatch beside a live operation",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        if snapshot.thread.owner_epoch != owner.cold_owner_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cold-recovery PermissionRequest owner epoch is stale",
            ));
        }
        let intent = match owner
            .capsule
            .restart_intent(&owner.thread_config_fingerprint)
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?
        {
            surface::DurableInteractionContinuationIntent::PermissionRetry(intent) => {
                intent.clone()
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cold-recovery permission owner contains a non-permission intent",
                ));
            }
        };
        let config = self.config.clone();
        let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
        let policy = crate::tool_execution::policy_for_tool_execution(&config);
        let cancel = CancelToken::new();
        let mut permission_overlay =
            runtime_permission_overlay_from_surface(intent.permission_overlay());
        let mut background_workflows = Vec::new();
        let mut state = self.state.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cold-recovery PermissionRequest thread state is unavailable",
            )
        })?;
        let result = (|| {
            let thread_extensions = state.thread.thread_extensions_handle();
            let turn_extensions = crate::extension::ExtensionData::new(format!(
                "{}:recovered-permission-retry",
                state.thread.thread_id()
            ));
            let extension_registry = crate::extension::empty_extension_registry();
            let goal_runtime_binding = state
                .thread
                .thread_extensions()
                .get::<crate::goal_actor::GoalRuntimeBinding>()
                .map(|binding| (*binding).clone());
            let goal_mode = Self::surface_operation_record(
                self.resident_surface.coordinator.state().snapshot(),
                owner.operation_id(),
            )
            .is_some_and(|operation| {
                matches!(
                    operation.intent.kind,
                    surface::OperationKind::GoalRun { .. }
                )
            });
            let mut sink = EventSink::new(io::sink(), config.output_format);
            let parts = state.thread.session_mut().runtime_parts();
            self.dispatch_recovered_permission_retry(
                &config,
                &mut state.events,
                &mut sink,
                &intent,
                &owner.historical_fence,
                owner
                    .capsule
                    .execution_context_fingerprint()
                    .expect("restartable permission capsule has a fingerprint"),
                &owner.thread_config_fingerprint,
                receipt,
                response,
                RecoveredToolExecutionDependencies {
                    cwd: &cwd,
                    subagent_depth: 0,
                    goal_mode,
                    policy: &policy,
                    instructions: parts.instructions,
                    memory: parts.memory,
                    mcp_registry: parts.mcp_registry,
                    hooks: parts.hooks,
                    cost_tracker: parts.cost_tracker,
                    cancel: &cancel,
                    task_registry: parts.task_registry,
                    background_workflows: &mut background_workflows,
                    workflow_ipc: None,
                    permission_overlay: &mut permission_overlay,
                    permission_handler: None,
                    user_input_handler: None,
                    mcp_elicitation_handler: None,
                    workflow_lifecycle_ingress: None,
                    wait_for_background_workflows: false,
                    extension_registry: Some(extension_registry.as_ref()),
                    extension_stores: Some(crate::extension::RuntimeExtensionStores::new(
                        thread_extensions.as_ref(),
                        &turn_extensions,
                    )),
                    goal_runtime_binding,
                    root_task_id: None,
                    child_budget: None,
                },
                crate::agent_loop::execute_child_agent_loop::<io::Sink>,
                crate::agent_loop::execute_child_agent_loop::<
                    crate::workflow::runner::SharedEventBuffer,
                >,
            )
        })();
        self.state = Some(state);
        result
    }

    /// Finish the interrupted historical operation after the recovered
    /// interaction has reached a durable terminal answer.
    pub(super) fn terminalize_cold_recovery_tool_approval(
        &mut self,
        owner: &ColdRecoveryToolApprovalOwner,
    ) -> io::Result<()> {
        self.terminalize_cold_recovery_operation(owner.operation_id(), "ToolApproval")
    }

    pub(super) fn terminalize_cold_recovery_operation(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
        interaction_label: &str,
    ) -> io::Result<()> {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let materialization = surface::MaterializationCause::ColdOwnerTakeover {
            new_incarnation: snapshot.cursor.incarnation.clone(),
            new_owner_epoch: snapshot.thread.owner_epoch,
        };
        loop {
            let before = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .cursor
                .clone();
            let action = self
                .resident_surface
                .coordinator
                .recover_operation(operation_id, &materialization)
                .map_err(|error| {
                    io::Error::other(format!(
                        "failed to terminalize recovered {interaction_label} operation: {error:?}"
                    ))
                })?;
            if matches!(
                action,
                surface::RecoveryAction::ExposeRecoveryRequired
                    | surface::RecoveryAction::ExposeRetryFinalization
                    | surface::RecoveryAction::ExposeRetryProjection
                    | surface::RecoveryAction::NoOp
            ) {
                return Ok(());
            }
            if self.resident_surface.coordinator.state().snapshot().cursor == before {
                return Err(io::Error::other(format!(
                    "recovered {interaction_label} terminalization made no durable progress"
                )));
            }
        }
    }

    pub(super) fn terminalize_cold_recovery_permission(
        &mut self,
        owner: &ColdRecoveryPermissionOwner,
    ) -> io::Result<()> {
        self.terminalize_cold_recovery_operation(owner.operation_id(), "PermissionRequest")
    }

    pub(super) fn settle_background_approval_resolution(
        &mut self,
        pending: &mut PendingBackgroundApprovalResolution,
    ) -> Result<(), RuntimeHostError> {
        let task_id = pending.task.task_id.as_str();
        let registry = self.handle.task_registry.clone();
        if Self::surface_operation_record(
            self.resident_surface.coordinator.state().snapshot(),
            &pending.fence.operation_id,
        )
        .is_some_and(|operation| operation.terminal.is_some())
        {
            return Ok(());
        }
        if pending.decision == surface::SurfaceAllowDeny::Deny && pending.pending_commit.is_some() {
            return self.settle_denied_background_approval(pending, &registry);
        }
        if pending.decision == surface::SurfaceAllowDeny::Deny {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            let denial_is_durably_underway =
                Self::surface_operation_record(snapshot, &pending.fence.operation_id)
                    .is_some_and(|operation| operation.finalization.is_some())
                    || snapshot.tasks.iter().any(|task| {
                        task.task_id == pending.task.task_id
                            && matches!(
                                task.status,
                                surface::SurfaceTaskStatus::Stopping
                                    | surface::SurfaceTaskStatus::Stopped
                                    | surface::SurfaceTaskStatus::Cancelled
                            )
                    });
            if denial_is_durably_underway {
                return self.settle_denied_background_approval(pending, &registry);
            }
        }
        let record = registry
            .get(task_id)
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval task disappeared".to_string(),
            })?;
        if record
            .pending_tool_call
            .as_ref()
            .is_none_or(|tool| tool.id != pending.tool.tool_call_id.as_str())
            || record.pending_provider_response.is_none()
        {
            if self.active.as_ref().is_some_and(|active| {
                active.surface_operation.as_ref().is_some_and(|fence| {
                    fence.operation_id == pending.fence.operation_id
                        && fence.generation_id > pending.fence.generation_id
                })
            }) {
                return Ok(());
            }
            return Err(RuntimeHostError::ThreadStartFailed {
                message: "background approval lost its exact pending provider response".to_string(),
            });
        }
        let approved = pending.decision == surface::SurfaceAllowDeny::Allow;
        match record.pending_tool_approval_response {
            Some(existing) if existing != approved => {
                return Err(RuntimeHostError::ThreadStartFailed {
                    message: "background approval decision conflicts with durable task state"
                        .to_string(),
                });
            }
            Some(_) => {}
            None => registry
                .submit_pending_tool_approval_response(task_id, approved)
                .map_err(|message| RuntimeHostError::ThreadStartFailed { message })?,
        }
        if !approved {
            return self.settle_denied_background_approval(pending, &registry);
        }
        self.resume_approved_background_operation(pending, &registry)
    }

    pub(super) fn commit_background_approval_stage(
        &mut self,
        pending: &mut PendingBackgroundApprovalResolution,
        prepared: PendingBackgroundApprovalCommit,
        context: &'static str,
    ) -> Result<(), RuntimeHostError> {
        if pending.pending_commit.is_none() {
            pending.pending_commit = Some(prepared);
        }
        let result = match pending
            .pending_commit
            .as_ref()
            .expect("background approval stage is retained before commit")
        {
            PendingBackgroundApprovalCommit::ProviderResume { fence, batch } => self
                .resident_surface
                .coordinator
                .commit_provider_background_resume_batch(fence.clone(), batch),
            PendingBackgroundApprovalCommit::Generation { fence, batch } => self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence.clone(), batch),
            PendingBackgroundApprovalCommit::ActorControl { fence, batch } => self
                .resident_surface
                .coordinator
                .commit_actor_background_control_batch(fence.clone(), batch),
            PendingBackgroundApprovalCommit::Actor { batch } => {
                self.resident_surface.coordinator.commit_actor_batch(batch)
            }
            PendingBackgroundApprovalCommit::Finalizer {
                operation_id,
                finalize_intent_id,
                terminal_commit_id: _,
                batch,
            } => self.resident_surface.coordinator.commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                batch,
            ),
            PendingBackgroundApprovalCommit::ActorFinalizerTaskTerminal {
                operation_id,
                finalize_intent_id,
                batch,
            } => self
                .resident_surface
                .coordinator
                .commit_actor_finalizer_task_terminal_batch(
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                    batch,
                ),
        };
        result.map_err(|error| RuntimeHostError::ThreadStartFailed {
            message: format!("{context}: {error:?}"),
        })?;
        pending.pending_commit = None;
        Ok(())
    }

    pub(super) fn resume_approved_background_operation(
        &mut self,
        pending: &mut PendingBackgroundApprovalResolution,
        registry: &TaskRegistry,
    ) -> Result<(), RuntimeHostError> {
        if self.active.as_ref().is_some_and(|active| {
            active.surface_operation.as_ref().is_some_and(|fence| {
                fence.operation_id == pending.fence.operation_id
                    && fence.generation_id > pending.fence.generation_id
            })
        }) {
            return Ok(());
        }
        let mut snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = Self::surface_operation_record(&snapshot, &pending.fence.operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval operation disappeared".to_string(),
            })?;
        let previous = operation
            .generations
            .iter()
            .find(|generation| generation.fence == pending.fence)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval predecessor generation disappeared".to_string(),
            })?;
        if snapshot
            .background_operations
            .iter()
            .any(|background| background.fence.operation_fence == pending.fence)
        {
            let task = snapshot
                .tasks
                .iter()
                .find(|task| task.task_id == pending.task.task_id)
                .cloned()
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "background approval projected task disappeared".to_string(),
                })?;
            if task.revision.get() != pending.task.task_revision.get().saturating_add(1)
                || task.backgrounded
                || task.background_fence.is_some()
                || task.status != surface::SurfaceTaskStatus::ApprovalRequired
            {
                return Err(RuntimeHostError::ThreadStartFailed {
                    message:
                        "background approval requires an explicit foreground task ownership commit"
                            .to_string(),
                });
            }
            let response_turn_id = registry
                .get(pending.task.task_id.as_str())
                .and_then(|task| task.pending_provider_response)
                .map(|response| response.completed().identity.turn_id.clone())
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "background approval response turn identity disappeared".to_string(),
                })?;
            let generation_id = surface::SurfaceGenerationId::new(
                previous
                    .fence
                    .generation_id
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                        message: "background approval generation id exhausted".to_string(),
                    })?,
            );
            let successor_fence = surface::SurfaceOperationFence {
                thread_id: snapshot.thread.thread_id.clone(),
                thread_owner_epoch: snapshot.thread.owner_epoch,
                operation_id: pending.fence.operation_id.clone(),
                generation_id,
            };
            let successor = surface::GenerationRecord {
                fence: successor_fence.clone(),
                logical_turn_id: response_turn_id,
                input: previous.input.clone(),
                predecessor: Some(previous.fence.clone()),
                attempt: surface::GenerationAttempt::RecoveryReplacement,
                goal_identity: None,
                replayability: previous.replayability.clone(),
                required_capabilities: previous.required_capabilities.clone(),
                capability_fingerprint: previous.capability_fingerprint.clone(),
                phase: surface::GenerationPhase::Reserved,
                started_witness: None,
                stop_reason: None,
            };
            let running_revision =
                surface::TaskRevision::try_new(task.revision.get().saturating_add(1))
                    .expect("task revision did not exhaust");
            let background_fence = pending.task.background_owner.clone().ok_or_else(|| {
                RuntimeHostError::ThreadStartFailed {
                    message: "background approval task lost its background owner".to_string(),
                }
            })?;
            let resume_batch = self.surface_event_batch_with_commit_id(
                vec![
                    (
                        surface::SurfaceScope::Background {
                            fence: background_fence.clone(),
                        },
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::GenerationReserved {
                                generation: successor,
                            },
                        ),
                    ),
                    (
                        surface::SurfaceScope::Operation {
                            operation_id: pending.fence.operation_id.clone(),
                        },
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::ControlIntentCommitted {
                                operation_id: pending.fence.operation_id.clone(),
                                request_id: operation.request_id.clone(),
                                intent: surface::PendingControlIntent::ResumeStarting {
                                    generation_fence: successor_fence,
                                },
                            },
                        ),
                    ),
                    (
                        surface::SurfaceScope::Thread,
                        surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                            task_id: task.task_id.clone(),
                            expected_revision: task.revision,
                            next_revision: running_revision,
                            status: surface::SurfaceTaskStatus::Running,
                            completed_at: None,
                            result: None,
                            error: None,
                        }),
                    ),
                ],
                None,
            );
            self.commit_background_approval_stage(
                pending,
                PendingBackgroundApprovalCommit::ProviderResume {
                    fence: background_fence,
                    batch: resume_batch,
                },
                "failed to durably reserve background approval continuation",
            )?;
            snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        }
        let recovery_rebased = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| {
                operation.operation_id == pending.fence.operation_id
                    && matches!(
                        operation.phase,
                        surface::OperationPhase::Suspended {
                            cause: surface::SuspensionCause::RecoveryRequired { .. }
                        }
                    )
                    && operation.generations.last().is_some_and(|generation| {
                        generation.phase == surface::GenerationPhase::Stopped
                            && generation.fence.generation_id > pending.fence.generation_id
                    })
            })
            .cloned();
        if let Some(operation) = recovery_rebased {
            let previous = operation
                .generations
                .last()
                .cloned()
                .expect("recovery-rebased operation has a generation");
            let response_turn_id = registry
                .get(pending.task.task_id.as_str())
                .and_then(|task| task.pending_provider_response)
                .map(|response| response.completed().identity.turn_id.clone())
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "recovered background approval response identity disappeared"
                        .to_string(),
                })?;
            let generation_id = surface::SurfaceGenerationId::new(
                previous
                    .fence
                    .generation_id
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                        message: "recovered background approval generation id exhausted"
                            .to_string(),
                    })?,
            );
            let successor_fence = surface::SurfaceOperationFence {
                thread_id: snapshot.thread.thread_id.clone(),
                thread_owner_epoch: snapshot.thread.owner_epoch,
                operation_id: pending.fence.operation_id.clone(),
                generation_id,
            };
            let successor = surface::GenerationRecord {
                fence: successor_fence.clone(),
                logical_turn_id: response_turn_id,
                input: previous.input.clone(),
                predecessor: Some(previous.fence.clone()),
                attempt: surface::GenerationAttempt::RecoveryReplacement,
                goal_identity: None,
                replayability: previous.replayability.clone(),
                required_capabilities: previous.required_capabilities.clone(),
                capability_fingerprint: previous.capability_fingerprint.clone(),
                phase: surface::GenerationPhase::Reserved,
                started_witness: None,
                stop_reason: None,
            };
            let resume_batch = self.surface_operation_batch(
                &operation.operation_id,
                vec![
                    surface::OperationPatch::GenerationReserved {
                        generation: successor,
                    },
                    surface::OperationPatch::ControlIntentCommitted {
                        operation_id: operation.operation_id.clone(),
                        request_id: operation.request_id.clone(),
                        intent: surface::PendingControlIntent::ResumeStarting {
                            generation_fence: successor_fence,
                        },
                    },
                ],
            );
            self.commit_background_approval_stage(
                pending,
                PendingBackgroundApprovalCommit::Actor {
                    batch: resume_batch,
                },
                "failed to reserve recovered background approval continuation",
            )?;
            snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        }
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == pending.fence.operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval continuation was not foregrounded".to_string(),
            })?;
        let successor = operation
            .generations
            .last()
            .cloned()
            .filter(|generation| generation.fence.generation_id > pending.fence.generation_id)
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval successor generation disappeared".to_string(),
            })?;
        if successor.phase == surface::GenerationPhase::Reserved {
            let started_commit_id =
                surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let started_batch = self.surface_operation_batch_with_commit_id(
                &operation.operation_id,
                vec![surface::OperationPatch::GenerationStarted {
                    fence: successor.fence.clone(),
                    witness: surface::GenerationStartedWitness {
                        started_commit_id: started_commit_id.clone(),
                        settings_revision: operation.intent.settings_revision,
                        policy_epoch: operation.intent.policy_epoch,
                        durable_replayability_digest: surface::canonical_replayability_digest(
                            &successor.replayability,
                        ),
                        capability_fingerprint: successor.capability_fingerprint.clone(),
                    },
                }],
                Some(started_commit_id),
            );
            self.commit_background_approval_stage(
                pending,
                PendingBackgroundApprovalCommit::Generation {
                    fence: successor.fence.clone(),
                    batch: started_batch,
                },
                "failed to start background approval continuation",
            )?;
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == pending.fence.operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "background approval continuation operation disappeared".to_string(),
            })?;
        let successor = operation.generations.last().cloned().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "background approval continuation generation disappeared".to_string(),
            }
        })?;
        if !operation
            .agent_loop_turns
            .iter()
            .any(|turn| turn.fence == successor.fence)
        {
            let loop_batch = self.surface_operation_batch(
                &operation.operation_id,
                vec![surface::OperationPatch::AgentLoopTurnStarted {
                    turn: surface::SurfaceAgentLoopTurn {
                        turn_id: successor.logical_turn_id.clone(),
                        fence: successor.fence.clone(),
                        ordinal: 0,
                        task_id: pending.task.task_id.clone(),
                        task_status: surface::SurfaceTaskRunningStatus::Running,
                    },
                }],
            );
            self.commit_background_approval_stage(
                pending,
                PendingBackgroundApprovalCommit::Generation {
                    fence: successor.fence.clone(),
                    batch: loop_batch,
                },
                "failed to start background approval agent loop",
            )?;
        }
        if self.active.is_none() {
            let interaction_command_tx = self.handle.command_tx.clone();
            let interaction_fence = successor.fence.clone();
            let request = HostedTurnRequest::new("")
                .with_operation_kind(HostedOperationKind::BackgroundContinuation {
                    task_id: pending.task.task_id.as_str().to_string(),
                })
                .with_goal_usage_tracking(true)
                .with_generation_handlers(move |_, cancel| {
                    HostedGenerationHandlers::default()
                        .with_provider_response_ingress(Arc::new(
                            RuntimeSurfaceProviderResponseIngress {
                                command_tx: interaction_command_tx.clone(),
                                fence: interaction_fence.clone(),
                            },
                        ))
                        .with_workflow_lifecycle_ingress(Arc::new(
                            RuntimeSurfaceWorkflowLifecycleIngress {
                                command_tx: interaction_command_tx.clone(),
                                fence: interaction_fence.clone(),
                            },
                        ))
                        .with_approval_handler(Arc::new(RuntimeSurfaceApprovalHandler {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                            cancel: cancel.clone(),
                        }))
                        .with_permission_handler(Arc::new(RuntimeSurfacePermissionHandler {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                            cancel: cancel.clone(),
                        }))
                        .with_user_input_handler(Arc::new(RuntimeSurfaceUserInputHandler {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                            cancel: cancel.clone(),
                        }))
                        .with_mcp_elicitation_handler(Arc::new(
                            RuntimeSurfaceMcpElicitationHandler {
                                command_tx: interaction_command_tx.clone(),
                                fence: interaction_fence.clone(),
                                cancel,
                            },
                        ))
                });
            let (start_tx, start_rx) = mpsc::sync_channel(1);
            self.handle_idle_command(ThreadCommand::StartTurn {
                request: Box::new(request),
                writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
                config: None,
                reply: start_tx,
            });
            start_rx
                .recv()
                .map_err(|_| RuntimeHostError::ThreadStartFailed {
                    message: "background approval continuation start channel closed".to_string(),
                })??;
            let active =
                self.active
                    .as_mut()
                    .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                        message: "background approval continuation did not become active"
                            .to_string(),
                    })?;
            active.surface_operation = Some(successor.fence);
        }
        Ok(())
    }

    pub(super) fn settle_denied_background_approval(
        &mut self,
        pending: &mut PendingBackgroundApprovalResolution,
        registry: &TaskRegistry,
    ) -> Result<(), RuntimeHostError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        if let Some(operation) =
            Self::surface_operation_record(&snapshot, &pending.fence.operation_id)
            && operation.terminal.is_some()
        {
            let _ = registry.finish_denied_pending_tool_approval(pending.task.task_id.as_str());
            return Ok(());
        }
        let operation = snapshot
            .operation_history
            .iter()
            .find(|operation| {
                operation.operation_id == pending.fence.operation_id && operation.terminal.is_none()
            })
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "denied background approval operation disappeared".to_string(),
            })?;
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == pending.task.task_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "denied background approval task disappeared".to_string(),
            })?;
        let background_fence = pending.task.background_owner.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "denied background approval lost its background owner".to_string(),
            }
        })?;
        let foreground_revision = surface::TaskRevision::try_new(
            pending
                .task
                .task_revision
                .get()
                .checked_add(1)
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "denied background approval task revision exhausted".to_string(),
                })?,
        )
        .map_err(|_| RuntimeHostError::ThreadStartFailed {
            message: "denied background approval task revision exhausted".to_string(),
        })?;
        let owns_original_background = task.backgrounded
            && task.background_fence.as_ref() == Some(&background_fence)
            && task.revision == pending.task.task_revision;
        let owns_explicit_foreground = !task.backgrounded
            && task.background_fence.is_none()
            && task.revision == foreground_revision;
        match task.status {
            surface::SurfaceTaskStatus::ApprovalRequired => {
                if !owns_original_background && !owns_explicit_foreground {
                    return Err(RuntimeHostError::ThreadStartFailed {
                        message:
                            "denied background approval task is not owned by its exact background or foreground claim"
                                .to_string(),
                    });
                }
                let stopping_revision =
                    surface::TaskRevision::try_new(task.revision.get().saturating_add(1))
                        .expect("task revision did not exhaust");
                let control_batch = self.surface_event_batch_with_commit_id(
                    vec![
                        (
                            surface::SurfaceScope::Background {
                                fence: background_fence.clone(),
                            },
                            surface::SurfaceEvent::Operation(
                                surface::OperationPatch::ControlIntentCommitted {
                                    operation_id: operation.operation_id.clone(),
                                    request_id: operation.request_id.clone(),
                                    intent: surface::PendingControlIntent::Terminalize {
                                        operation_id: operation.operation_id.clone(),
                                        cause: surface::TerminalizationCause::UserCancel,
                                    },
                                },
                            ),
                        ),
                        (
                            surface::SurfaceScope::Thread,
                            surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                                task_id: task.task_id.clone(),
                                expected_revision: task.revision,
                                next_revision: stopping_revision,
                                status: surface::SurfaceTaskStatus::Stopping,
                                completed_at: None,
                                result: None,
                                error: None,
                            }),
                        ),
                    ],
                    None,
                );
                self.commit_background_approval_stage(
                    pending,
                    PendingBackgroundApprovalCommit::ActorControl {
                        fence: background_fence.clone(),
                        batch: control_batch,
                    },
                    "failed to commit denied background approval control",
                )?;
            }
            surface::SurfaceTaskStatus::Stopping
            | surface::SurfaceTaskStatus::Cancelled
            | surface::SurfaceTaskStatus::Stopped => {}
            _ => {
                return Err(RuntimeHostError::ThreadStartFailed {
                    message: "denied background approval task left its terminalization path"
                        .to_string(),
                });
            }
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .operation_history
            .iter()
            .find(|operation| {
                operation.operation_id == pending.fence.operation_id && operation.terminal.is_none()
            })
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "denied background approval operation disappeared after control"
                    .to_string(),
            })?;
        let (finalize_intent_id, terminal_commit_id) =
            if let Some(PendingBackgroundApprovalCommit::Finalizer {
                finalize_intent_id,
                terminal_commit_id,
                ..
            }) = pending.pending_commit.as_ref()
            {
                (finalize_intent_id.clone(), terminal_commit_id.clone())
            } else if let Some(finalization) = operation.finalization.as_ref() {
                (
                    finalization.finalize_intent_id.clone(),
                    finalization.terminal_commit_id.clone(),
                )
            } else {
                let finalize_intent_id = surface::SurfaceFinalizeIntentId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7");
                let terminal_commit_id =
                    surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7");
                let suspended_cause = surface::SuspendedFinalizationCause::Terminalization(
                    surface::TerminalizationCause::UserCancel,
                );
                let finalization_batch = self.surface_event_batch_with_commit_id(
                    vec![(
                        surface::SurfaceScope::Background {
                            fence: background_fence.clone(),
                        },
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::FinalizationStarted {
                                operation_id: operation.operation_id.clone(),
                                finalize_intent_id: finalize_intent_id.clone(),
                                terminal_commit_id: terminal_commit_id.clone(),
                                selected_cause: surface::OperationFinalizationCause::Suspended(
                                    suspended_cause.clone(),
                                ),
                                suspended_cause: Some(suspended_cause),
                                expected_settlements: Vec::new(),
                            },
                        ),
                    )],
                    None,
                );
                self.commit_background_approval_stage(
                    pending,
                    PendingBackgroundApprovalCommit::Finalizer {
                        operation_id: operation.operation_id.clone(),
                        finalize_intent_id: finalize_intent_id.clone(),
                        terminal_commit_id: terminal_commit_id.clone(),
                        batch: finalization_batch,
                    },
                    "failed to begin denied background approval finalization",
                )?;
                (finalize_intent_id, terminal_commit_id)
            };
        let terminal = surface::OperationTerminal::Cancelled {
            reason: surface::CancelReason::User,
        };
        let terminal_record = surface::OperationTerminalRecord {
            operation_id: operation.operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal: terminal.clone(),
            usage: surface::UsageTotals {
                input_tokens: 0,
                output_tokens: 0,
                cache_tokens: 0,
                estimated_cost_usd_micros: 0,
            },
            source_diagnostic_digest: None,
            settlement_receipts: Vec::new(),
            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                "interaction terminal has no verifier proof",
            ),
            committed_at: surface::UnixMillis::new(0),
        };
        let projection = prepare_main_session_task_terminal_projection(
            self.resident_surface.coordinator.state().snapshot(),
            &operation.operation_id,
            &terminal_record,
        )?
        .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
            message: "denied background approval lost its main-session task projection".to_string(),
        })?;
        let mut terminal_events = vec![projection.event.clone()];
        terminal_events.push((
            surface::SurfaceScope::Background {
                fence: background_fence,
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal {
                record: terminal_record.clone(),
            }),
        ));
        let terminal_batch = self
            .surface_event_batch_with_commit_id(terminal_events, Some(terminal_commit_id.clone()));
        self.commit_background_approval_stage(
            pending,
            PendingBackgroundApprovalCommit::ActorFinalizerTaskTerminal {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id,
                batch: terminal_batch.clone(),
            },
            "failed to commit denied background approval terminal",
        )?;
        mirror_main_session_task_terminal_projection(registry, &terminal_record, &projection);
        self.cache_surface_terminal(surface::OperationTerminalAtCursor {
            operation_id: operation.operation_id,
            terminal,
            completion_proof: terminal_record.completion_proof.clone(),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        });
        let _ = registry.finish_denied_pending_tool_approval(pending.task.task_id.as_str());
        Ok(())
    }

    pub(super) fn uncommitted_interaction_response(
        request_id: surface::SurfaceRequestId,
        interaction: &ResidentSurfaceInteraction,
        code: surface::SurfaceMutationErrorCode,
        message: &'static str,
    ) -> surface::MutationReply<surface::RespondInteractionOutput> {
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Invalid {
                request_id,
                target: Some(surface::MutationTarget::Interaction {
                    thread_id: interaction.record.thread_id.clone(),
                    interaction_id: interaction.record.interaction_id.clone(),
                }),
                error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: Some(surface::SurfaceMutationRevision::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction.record.interaction_id.clone(),
                        revision: interaction.revision,
                        route_epoch: interaction_route_epoch(&interaction.route),
                    }),
                }),
            },
        }
    }

    pub(super) fn stale_interaction_response(
        request_id: surface::SurfaceRequestId,
        interaction: &ResidentSurfaceInteraction,
        code: surface::SurfaceMutationErrorCode,
        message: &'static str,
    ) -> surface::MutationReply<surface::RespondInteractionOutput> {
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Stale {
                request_id,
                target: Some(surface::MutationTarget::Interaction {
                    thread_id: interaction.record.thread_id.clone(),
                    interaction_id: interaction.record.interaction_id.clone(),
                }),
                error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: Some(surface::SurfaceMutationRevision::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction.record.interaction_id.clone(),
                        revision: interaction.revision,
                        route_epoch: interaction_route_epoch(&interaction.route),
                    }),
                }),
            },
        }
    }

    pub(super) fn prepare_surface_terminalization(
        &self,
        fence: &surface::SurfaceOperationFence,
        request_id: surface::SurfaceRequestId,
        cause: surface::TerminalizationCause,
    ) -> Result<PreparedSurfaceTerminalization, surface::SurfaceClientCommandError> {
        if self
            .resident_surface
            .interactions
            .values()
            .any(|interaction| {
                &interaction.record.fence == fence
                    && interaction.winning_receipt.is_none()
                    && interaction.private_response.is_some()
            })
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let reason = match cause {
            surface::TerminalizationCause::HostShutdown => {
                surface::InteractionCancelReason::HostShutdown
            }
            surface::TerminalizationCause::ThreadClose => {
                surface::InteractionCancelReason::ThreadClose
            }
            surface::TerminalizationCause::UserCancel => {
                surface::InteractionCancelReason::OperationCancelled {
                    reason: surface::CancelReason::User,
                }
            }
            surface::TerminalizationCause::GoalPause => {
                surface::InteractionCancelReason::OperationCancelled {
                    reason: surface::CancelReason::GoalPause,
                }
            }
        };
        let mut interactions = self
            .resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                &interaction.record.fence == fence
                    && interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
            })
            .map(|(interaction_id, interaction)| (interaction_id.clone(), interaction.revision))
            .collect::<Vec<_>>();
        interactions.sort_by_key(|(interaction_id, _)| interaction_id.clone());
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let mut capability_calls = self
            .resident_surface
            .capability
            .durable_calls(snapshot)
            .filter(|(call, _, _)| &call.fence == fence)
            .collect::<Vec<_>>();
        capability_calls.sort_by_key(|(call, _, _)| call.call_id.clone());
        let mut events = vec![(
            surface::SurfaceScope::Operation {
                operation_id: fence.operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::ControlIntentCommitted {
                operation_id: fence.operation_id.clone(),
                request_id,
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: fence.operation_id.clone(),
                    cause,
                },
            }),
        )];
        for (interaction_id, expected_revision) in &interactions {
            let next_revision =
                surface::InteractionRevision::try_new(expected_revision.get().saturating_add(1))
                    .expect("interaction revision did not exhaust");
            // Detached child permission interactions are thread-owned after
            // the parent generation has ended. Classify from the durable
            // projection so malformed or stale resident entries stay on the
            // generation path and fail closed in the normal authority check.
            let interaction_scope = snapshot
                .interactions
                .iter()
                .find(|interaction| &interaction.interaction_id == interaction_id)
                .filter(|interaction| {
                    surface::detached_child_permission_interaction_matches(
                        snapshot,
                        interaction,
                        true,
                        false,
                    )
                })
                .map(|_| surface::SurfaceScope::Thread)
                .unwrap_or_else(|| surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                });
            events.push((
                interaction_scope,
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: interaction_id.clone(),
                    expected_revision: *expected_revision,
                    next_revision,
                    reason: reason.clone(),
                }),
            ));
        }
        let mut terminalized_capability_calls = Vec::new();
        let mut terminalized_tool_ids = BTreeSet::new();
        let mut terminalized_tool_calls = Vec::new();
        let covered_terminal_leases = capability_calls
            .iter()
            .filter_map(|(_, lease, _)| lease.as_ref().map(|lease| lease.lease_id.clone()))
            .collect::<BTreeSet<_>>();
        for (mut call, terminal_cleanup_lease, write_claimed) in capability_calls {
            let diagnostic = surface::SafeDiagnosticText::try_new(match cause {
                surface::TerminalizationCause::HostShutdown => {
                    "ACP capability cancelled by host shutdown"
                }
                surface::TerminalizationCause::ThreadClose => {
                    "ACP capability cancelled by thread close"
                }
                surface::TerminalizationCause::UserCancel => "ACP capability cancelled by user",
                surface::TerminalizationCause::GoalPause => {
                    "ACP capability cancelled by Goal pause"
                }
            })
            .expect("fixed capability cancellation diagnostic is bounded");
            let terminal_cleanup_was_prepared = matches!(
                (&call.kind, &call.state),
                (
                    surface::SurfaceCapabilityCallKind::TerminalKill
                        | surface::SurfaceCapabilityCallKind::TerminalRelease,
                    surface::SurfaceCapabilityCallState::Prepared,
                )
            );
            call.state = match (&call.kind, &call.state) {
                (
                    surface::SurfaceCapabilityCallKind::ReadTextFile
                    | surface::SurfaceCapabilityCallKind::TerminalOutput
                    | surface::SurfaceCapabilityCallKind::TerminalWaitForExit
                    | surface::SurfaceCapabilityCallKind::WriteTextFile
                    | surface::SurfaceCapabilityCallKind::TerminalCreate,
                    surface::SurfaceCapabilityCallState::Prepared,
                ) if !write_claimed => {
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic }
                }
                (
                    surface::SurfaceCapabilityCallKind::ReadTextFile
                    | surface::SurfaceCapabilityCallKind::TerminalOutput
                    | surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
                    surface::SurfaceCapabilityCallState::Prepared,
                ) => surface::SurfaceCapabilityCallState::ObservationUnavailable {
                    error: diagnostic,
                },
                (
                    surface::SurfaceCapabilityCallKind::ReadTextFile
                    | surface::SurfaceCapabilityCallKind::TerminalOutput
                    | surface::SurfaceCapabilityCallKind::TerminalWaitForExit,
                    surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => surface::SurfaceCapabilityCallState::ObservationUnavailable {
                    error: diagnostic,
                },
                (
                    surface::SurfaceCapabilityCallKind::WriteTextFile,
                    surface::SurfaceCapabilityCallState::DeliveryPossible
                    | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::FileWrite,
                    error: diagnostic,
                },
                (
                    surface::SurfaceCapabilityCallKind::TerminalCreate,
                    surface::SurfaceCapabilityCallState::DeliveryPossible
                    | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::TerminalCreate,
                    error: diagnostic,
                },
                (
                    surface::SurfaceCapabilityCallKind::TerminalKill,
                    surface::SurfaceCapabilityCallState::Prepared
                    | surface::SurfaceCapabilityCallState::DeliveryPossible
                    | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::TerminalKill,
                    error: diagnostic,
                },
                (
                    surface::SurfaceCapabilityCallKind::TerminalRelease,
                    surface::SurfaceCapabilityCallState::Prepared
                    | surface::SurfaceCapabilityCallState::DeliveryPossible
                    | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::TerminalRelease,
                    error: diagnostic,
                },
                _ => continue,
            };
            if terminal_cleanup_was_prepared {
                let mut delivery_possible = call.clone();
                delivery_possible.state = surface::SurfaceCapabilityCallState::DeliveryPossible;
                events.push((
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Tool(surface::ToolPatch::CapabilityCallChanged {
                        call: delivery_possible,
                    }),
                ));
            }
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Tool(surface::ToolPatch::CapabilityCallChanged {
                    call: call.clone(),
                }),
            ));
            if matches!(
                &call.state,
                surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::TerminalCreate,
                    ..
                }
            ) {
                events.push((
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Tool(surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: surface::SurfaceRemoteTerminalLease {
                            lease_id: ResidentCapabilityController::terminal_create_lease_id(
                                &call.call_id,
                            ),
                            owning_tool_call_id: call.owning_tool_call_id.clone(),
                            state: surface::SurfaceRemoteTerminalLeaseState::IdentityUnknown {
                                create_call_id: call.call_id.clone(),
                            },
                        },
                    }),
                ));
            } else if matches!(
                &call.state,
                surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: surface::ExternalEffectKind::TerminalKill
                        | surface::ExternalEffectKind::TerminalRelease,
                    ..
                }
            ) {
                let terminal_cleanup_lease = terminal_cleanup_lease
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let lease = snapshot
                    .tools
                    .iter()
                    .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
                    .and_then(|tool| {
                        tool.terminal_leases.iter().find(|lease| {
                            lease.lease_id == terminal_cleanup_lease.lease_id
                                && matches!(
                                    &lease.state,
                                    surface::SurfaceRemoteTerminalLeaseState::KillPending {
                                        terminal_id,
                                        owner_fence,
                                    } | surface::SurfaceRemoteTerminalLeaseState::ReleasePending {
                                        terminal_id,
                                        owner_fence,
                                    } if owner_fence == fence
                                        && terminal_id == &terminal_cleanup_lease.terminal_id
                                )
                        })
                    })
                    .cloned()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let terminal_id = match lease.state {
                    surface::SurfaceRemoteTerminalLeaseState::KillPending {
                        terminal_id, ..
                    }
                    | surface::SurfaceRemoteTerminalLeaseState::ReleasePending {
                        terminal_id,
                        ..
                    } => terminal_id,
                    _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
                };
                events.push((
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Tool(surface::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: surface::SurfaceRemoteTerminalLease {
                            lease_id: lease.lease_id,
                            owning_tool_call_id: call.owning_tool_call_id.clone(),
                            state: surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                terminal_id: Some(terminal_id),
                                owner_fence: fence.clone(),
                            },
                        },
                    }),
                ));
            }
            if matches!(
                call.state,
                surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
            ) && terminalized_tool_ids.insert(call.owning_tool_call_id.clone())
            {
                terminalized_tool_calls.push(call.clone());
            }
            terminalized_capability_calls.push(call);
        }
        let mut uncovered_live_leases = snapshot
            .tools
            .iter()
            .flat_map(|tool| {
                tool.terminal_leases.iter().filter_map(|lease| {
                    let surface::SurfaceRemoteTerminalLeaseState::Live {
                        terminal_id,
                        owner_fence,
                    } = &lease.state
                    else {
                        return None;
                    };
                    if owner_fence != fence || covered_terminal_leases.contains(&lease.lease_id) {
                        return None;
                    }
                    Some((
                        lease.lease_id.clone(),
                        tool.request.tool_call_id.clone(),
                        terminal_id.clone(),
                    ))
                })
            })
            .collect::<Vec<_>>();
        uncovered_live_leases.sort_by_key(|(lease_id, _, _)| lease_id.clone());
        for (lease_id, tool_call_id, terminal_id) in uncovered_live_leases {
            let template = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == tool_call_id)
                .and_then(|tool| {
                    tool.capability_calls.iter().rev().find(|call| {
                        call.fence == *fence
                            && call.kind == surface::SurfaceCapabilityCallKind::TerminalCreate
                            && matches!(
                                &call.state,
                                surface::SurfaceCapabilityCallState::Completed {
                                    result:
                                        surface::CapabilityCallResult::TerminalCreated {
                                            terminal_id: created_terminal_id,
                                        },
                                    ..
                                } if created_terminal_id == &terminal_id
                            )
                    })
                })
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let mut call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: template.acp_session_id.clone(),
                fence: fence.clone(),
                capability_revision: template.capability_revision,
                policy_epoch: template.policy_epoch,
                kind: surface::SurfaceCapabilityCallKind::TerminalKill,
                arguments_digest: surface_sha256(terminal_id.as_str().as_bytes()),
                owning_tool_call_id: tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Tool(surface::ToolPatch::CapabilityCallChanged {
                    call: call.clone(),
                }),
            ));
            call.state = surface::SurfaceCapabilityCallState::DeliveryPossible;
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Tool(surface::ToolPatch::CapabilityCallChanged {
                    call: call.clone(),
                }),
            ));
            let diagnostic = surface::SafeDiagnosticText::try_new(
                "runtime terminalized before remote terminal cleanup was admitted",
            )
            .expect("fixed terminal cleanup diagnostic is bounded");
            call.state = surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                effect_kind: surface::ExternalEffectKind::TerminalKill,
                error: diagnostic,
            };
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Tool(surface::ToolPatch::CapabilityCallChanged {
                    call: call.clone(),
                }),
            ));
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Tool(surface::ToolPatch::RemoteTerminalLeaseChanged {
                    lease: surface::SurfaceRemoteTerminalLease {
                        lease_id,
                        owning_tool_call_id: call.owning_tool_call_id.clone(),
                        state: surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                            terminal_id: Some(terminal_id),
                            owner_fence: fence.clone(),
                        },
                    },
                }),
            ));
            if terminalized_tool_ids.insert(call.owning_tool_call_id.clone()) {
                terminalized_tool_calls.push(call.clone());
            }
            terminalized_capability_calls.push(call);
        }
        for call in terminalized_tool_calls {
            events.extend(self.ambiguous_capability_tool_events(&call)?);
        }
        Ok(PreparedSurfaceTerminalization {
            fence: fence.clone(),
            cause,
            batch: self.surface_event_batch_with_commit_id(events, None),
            interaction_ids: interactions
                .into_iter()
                .map(|(interaction_id, _)| interaction_id)
                .collect(),
            capability_call_ids: terminalized_capability_calls
                .into_iter()
                .map(|call| call.call_id)
                .collect(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        })
    }

    pub(super) fn apply_surface_interaction_cancellations(
        &mut self,
        interaction_ids: &[surface::SurfaceInteractionId],
    ) {
        for interaction_id in interaction_ids {
            let cold_recovery_owner = self
                .resident_surface
                .interactions
                .cold_recovery_owners
                .remove(interaction_id);
            let cold_recovery_permission_owner = self
                .resident_surface
                .interactions
                .cold_recovery_permission_owners
                .remove(interaction_id);
            let continuation_turn_owner = self
                .resident_surface
                .interactions
                .continuation_turn_owners
                .remove(interaction_id);
            let waiter = self
                .resident_surface
                .interactions
                .remove(interaction_id)
                .expect("committed interaction remains resident")
                .waiter;
            if let Some(waiter) = waiter {
                match waiter {
                    ResidentInteractionWaiter::ToolApproval { waiter, .. } => {
                        let _ = waiter.send(Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "tool approval was cancelled before resolution",
                        )));
                    }
                    ResidentInteractionWaiter::Permission(waiter) => {
                        let _ = waiter.send(Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "permission request was cancelled before resolution",
                        )));
                    }
                    ResidentInteractionWaiter::UserInput(waiter) => {
                        let _ = waiter.send(Ok(None));
                    }
                    ResidentInteractionWaiter::McpElicitation(waiter) => {
                        let _ = waiter.send(Ok(orca_mcp::McpElicitationResponse::Decline));
                    }
                    ResidentInteractionWaiter::DetachedPermission {
                        registry,
                        key,
                        request_digest,
                    } => {
                        if let Err(error) = resolve_detached_permission_deny(
                            &registry,
                            &key,
                            &request_digest,
                            "permission request was cancelled before resolution",
                        ) {
                            eprintln!(
                                "orca: detached cancellation denial remains pending for {key}: {error}"
                            );
                        }
                    }
                }
            }
            if let Some(owner) = cold_recovery_owner
                && let Err(error) = self.terminalize_cold_recovery_tool_approval(&owner)
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cancelled cold-recovery ToolApproval terminalization failed: {error}"
                ));
            }
            if let Some(owner) = cold_recovery_permission_owner
                && let Err(error) = self.terminalize_cold_recovery_permission(&owner)
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cancelled cold-recovery PermissionRequest terminalization failed: {error}"
                ));
            }
            if let Some(owner) = continuation_turn_owner
                && let Err(error) =
                    self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation")
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "cancelled cold-recovery continuation terminalization failed: {error}"
                ));
            }
        }
    }

    pub(super) fn apply_surface_capability_cancellations(
        &mut self,
        call_ids: &[surface::SurfaceCapabilityCallId],
    ) {
        for effect in self.resident_surface.capability.cancel_calls(call_ids) {
            apply_runtime_actor_reply_effect(effect);
        }
    }

    pub(super) fn abandon_surface_capability_waiters_for_cold_recovery(&mut self) {
        self.resident_surface.capability.abandon_call_waiters();
    }

    pub(super) fn prepare_surface_attachment_transition(
        &self,
        attachment_id: &surface::SurfaceAttachmentId,
    ) -> Result<Option<PreparedSurfaceAttachmentTransition>, ()> {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let mut affected = self
            .resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
                    && interaction_route_admits(&interaction.route, attachment_id)
            })
            .map(|(interaction_id, interaction)| {
                (
                    interaction_id.clone(),
                    interaction.record.kind,
                    interaction.record.fence.clone(),
                    interaction.revision,
                    interaction_route_epoch(&interaction.route),
                    is_detached_child_permission_interaction(snapshot, interaction),
                )
            })
            .collect::<Vec<_>>();
        affected.sort_by_key(|(interaction_id, ..)| interaction_id.clone());
        let Some((_, _, fence, _, _, detached_permission)) = affected.first() else {
            return Ok(None);
        };
        let fence = fence.clone();
        // A detached permission route is Thread-scoped and uses actor
        // authority. Keep it out of a mixed Generation batch with foreground
        // interactions; the next reconciliation pass handles the other scope.
        let detached_permission = *detached_permission;
        affected.retain(|(_, _, _, _, _, detached)| *detached == detached_permission);
        if affected
            .iter()
            .any(|(_, _, candidate, _, _, _)| candidate != &fence)
        {
            return Err(());
        }
        let mut events = Vec::new();
        let mut interactions = Vec::new();
        let mut affected_route_epochs = Vec::new();
        for (interaction_id, kind, _, expected_revision, current_epoch, detached) in affected {
            let route_revision = surface::InteractionRevision::try_new(expected_revision.get() + 1)
                .expect("interaction revision did not exhaust");
            let next_epoch = surface::ResponseRouteEpoch::try_new(current_epoch.get() + 1)
                .expect("route epoch did not exhaust");
            let fallback = self
                .resident_surface
                .hub
                .select_interaction_attachment_excluding(kind, None, Some(attachment_id));
            let (private_route, public_route, cancelled) = match fallback {
                Some(fallback) => (
                    surface::BrokerInteractionResponseRoute::Exclusive {
                        epoch: next_epoch,
                        attachment_id: fallback.clone(),
                        grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
                    },
                    surface::SurfaceInteractionRoute::Exclusive {
                        epoch: next_epoch,
                        attachment_id: fallback,
                    },
                    false,
                ),
                None => (
                    surface::BrokerInteractionResponseRoute::Unassigned { epoch: next_epoch },
                    surface::SurfaceInteractionRoute::Unassigned { epoch: next_epoch },
                    !detached,
                ),
            };
            let event_scope = if detached {
                surface::SurfaceScope::Thread
            } else {
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                }
            };
            events.push((
                event_scope.clone(),
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::RouteChanged {
                    interaction_id: interaction_id.clone(),
                    expected_revision,
                    next_revision: route_revision,
                    route: public_route,
                }),
            ));
            let revision = if cancelled {
                let cancelled_revision =
                    surface::InteractionRevision::try_new(route_revision.get() + 1)
                        .expect("interaction revision did not exhaust");
                events.push((
                    event_scope,
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                        interaction_id: interaction_id.clone(),
                        expected_revision: route_revision,
                        next_revision: cancelled_revision,
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                    }),
                ));
                cancelled_revision
            } else {
                route_revision
            };
            affected_route_epochs.push((interaction_id.clone(), next_epoch));
            interactions.push(PreparedSurfaceDetachInteraction {
                interaction_id,
                revision,
                route: private_route,
                cancelled,
            });
        }
        let commit_id = surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
            .expect("generated UUID is v7");
        let batch = self.surface_event_batch_with_commit_id(events, Some(commit_id.clone()));
        Ok(Some(PreparedSurfaceAttachmentTransition {
            fence,
            batch,
            commit_id,
            affected_route_epochs,
            interactions,
        }))
    }

    pub(super) fn apply_surface_attachment_transition(
        &mut self,
        active: Option<&mut ActiveOperation>,
        transition: &PreparedSurfaceAttachmentTransition,
    ) {
        let mut cancelled_waiters = Vec::new();
        let mut cancelled_continuation_owners = Vec::new();
        for prepared in &transition.interactions {
            if prepared.cancelled {
                if let Some(owner) = self
                    .resident_surface
                    .interactions
                    .continuation_turn_owners
                    .remove(&prepared.interaction_id)
                {
                    cancelled_continuation_owners.push(owner);
                }
                if let Some(waiter) = self
                    .resident_surface
                    .interactions
                    .remove(&prepared.interaction_id)
                    .expect("committed interaction remains resident")
                    .waiter
                {
                    cancelled_waiters.push(waiter);
                }
            } else {
                let interaction = self
                    .resident_surface
                    .interactions
                    .get_mut(&prepared.interaction_id)
                    .expect("committed interaction remains resident");
                interaction.revision = prepared.revision;
                interaction.route = prepared.route.clone();
            }
        }
        if !cancelled_waiters.is_empty()
            && let Some(active) = active
        {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            active.surface_execution_failure_diagnostic = None;
        }
        for waiter in cancelled_waiters {
            match waiter {
                ResidentInteractionWaiter::ToolApproval { waiter, .. } => {
                    let _ = waiter.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "tool approval capability became unavailable",
                    )));
                }
                ResidentInteractionWaiter::Permission(waiter) => {
                    let _ = waiter.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "permission capability became unavailable",
                    )));
                }
                ResidentInteractionWaiter::UserInput(waiter) => {
                    let _ = waiter.send(Ok(None));
                }
                ResidentInteractionWaiter::McpElicitation(waiter) => {
                    let _ = waiter.send(Ok(orca_mcp::McpElicitationResponse::Decline));
                }
                ResidentInteractionWaiter::DetachedPermission {
                    registry,
                    key,
                    request_digest,
                } => {
                    if let Err(error) = resolve_detached_permission_deny(
                        &registry,
                        &key,
                        &request_digest,
                        "permission capability became unavailable",
                    ) {
                        eprintln!(
                            "orca: detached capability-loss denial remains pending for {key}: {error}"
                        );
                    }
                }
            }
        }
        for owner in cancelled_continuation_owners {
            if let Err(error) =
                self.terminalize_cold_recovery_operation(owner.operation_id(), "continuation")
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "capability-lost cold-recovery continuation terminalization failed: {error}"
                ));
            }
        }
    }

    pub(super) fn next_invalid_surface_interaction_attachment(
        &self,
    ) -> Option<surface::SurfaceAttachmentId> {
        self.resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
            })
            .flat_map(|(interaction_id, interaction)| {
                interaction_route_attachments(&interaction.route)
                    .into_iter()
                    .filter(|attachment_id| {
                        !self
                            .resident_surface
                            .hub
                            .admits_interaction_attachment(attachment_id, interaction.record.kind)
                    })
                    .map(|attachment_id| (interaction_id.clone(), attachment_id))
            })
            .min()
            .map(|(_, attachment_id)| attachment_id)
    }

    pub(super) fn reconcile_surface_interaction_capabilities(
        &mut self,
        mut active: Option<&mut ActiveOperation>,
    ) {
        if !self
            .resident_surface
            .interactions
            .pending_detaches
            .is_empty()
            || !self
                .resident_surface
                .interactions
                .pending_capability_losses
                .is_empty()
        {
            return;
        }
        while let Some(attachment_id) = self.next_invalid_surface_interaction_attachment() {
            let transition = match self.prepare_surface_attachment_transition(&attachment_id) {
                Ok(Some(transition)) => transition,
                Ok(None) | Err(()) => return,
            };
            let thread_scoped = transition
                .batch
                .events
                .as_slice()
                .iter()
                .all(|event| matches!(event.scope, surface::SurfaceScope::Thread));
            let committed = if thread_scoped {
                self.resident_surface
                    .coordinator
                    .commit_actor_batch(&transition.batch)
            } else {
                self.resident_surface
                    .coordinator
                    .commit_generation_batch(transition.fence.clone(), &transition.batch)
            };
            if committed.is_err() {
                eprintln!("orca: typed interaction capability-loss commit failed");
                self.resident_surface
                    .interactions
                    .pending_capability_losses
                    .insert(
                        attachment_id,
                        PendingSurfaceCapabilityLoss {
                            transition,
                            retry_at: tokio::time::Instant::now()
                                + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                        },
                    );
                return;
            }
            self.apply_surface_attachment_transition(active.as_deref_mut(), &transition);
        }
    }
}

fn resolve_detached_permission_deny(
    registry: &TaskRegistry,
    key: &str,
    request_digest: &surface::Sha256Digest,
    reason: &str,
) -> Result<(), String> {
    let response = crate::runtime_permission::RuntimePermissionResponse {
        decision: crate::protocol::PermissionResponseDecision::Deny,
        scope: crate::protocol::PermissionGrantScope::Turn,
        permissions: crate::protocol::RequestPermissionProfile::default(),
        strict_auto_review: false,
    };
    registry
        .resolve_detached_permission_request(key, request_digest, response)
        .map_err(|error| {
            format!("failed to persist detached permission denial ({reason}): {error}")
        })
}

fn detached_permission_response_from_receipt(
    receipt: &surface::SurfaceInteractionResolutionReceipt,
) -> Option<crate::runtime_permission::RuntimePermissionResponse> {
    let surface::SurfaceInteractionSafeProjection::PermissionRequest {
        decision,
        scope,
        permissions,
        strict_auto_review,
    } = &receipt.safe_projection
    else {
        return None;
    };
    Some(crate::runtime_permission::RuntimePermissionResponse {
        decision: match decision {
            surface::SurfaceAllowDeny::Allow => crate::protocol::PermissionResponseDecision::Allow,
            surface::SurfaceAllowDeny::Deny => crate::protocol::PermissionResponseDecision::Deny,
        },
        scope: match scope {
            surface::PermissionGrantScope::Turn => crate::protocol::PermissionGrantScope::Turn,
            surface::PermissionGrantScope::Session => {
                crate::protocol::PermissionGrantScope::Session
            }
        },
        permissions: runtime_permission_profile_from_surface(permissions),
        strict_auto_review: *strict_auto_review,
    })
}

fn detached_surface_request_matches_mailbox(
    surface_request: &surface::SurfaceInteractionRequest,
    mailbox_request: &crate::runtime_permission::RuntimePermissionRequest,
    binding: &crate::tasks::DetachedSubagentBinding,
    permissions: &surface::SurfacePermissionProfile,
    authority: &surface::AuthorityFingerprint,
    expected_task_revision: surface::TaskRevision,
) -> bool {
    let surface::SurfaceInteractionRequest::PermissionRequest {
        tool_call_id,
        context,
        reason,
        permissions: observed_permissions,
        authority: observed_authority,
    } = surface_request
    else {
        return false;
    };
    if !detached_context_matches_mailbox(context, mailbox_request, binding, expected_task_revision)
    {
        return false;
    }
    reason.as_ref().map(|value| value.as_str()) == mailbox_request.reason.as_deref()
        && observed_permissions == permissions
        && observed_authority == authority
        && detached_mailbox_tool_call_matches(mailbox_request, tool_call_id)
}

fn detached_context_matches_mailbox(
    context: &surface::SurfacePermissionContext,
    mailbox_request: &crate::runtime_permission::RuntimePermissionRequest,
    binding: &crate::tasks::DetachedSubagentBinding,
    expected_task_revision: surface::TaskRevision,
) -> bool {
    let surface::SurfacePermissionOwnerRef::Child {
        task_id,
        task_revision,
        agent_id,
        agent_revision,
        activity_id,
        turn_id,
        tool_call_id,
    } = &context.owner
    else {
        return false;
    };
    let crate::runtime_permission::RuntimePermissionContext::Child {
        task_id: mailbox_task_id,
        task_revision: mailbox_task_revision,
        agent_id: mailbox_agent_id,
        agent_revision: mailbox_agent_revision,
        activity_id: mailbox_activity_id,
        turn_id: mailbox_turn_id,
        tool_call_id: mailbox_tool_call_id,
        origin,
    } = &mailbox_request.context
    else {
        return false;
    };
    context.origin == surface::SurfacePermissionOrigin::ChildAgent
        && *origin == surface::SurfacePermissionOrigin::ChildAgent
        && task_id == mailbox_task_id
        && *task_revision >= *mailbox_task_revision
        && *task_revision <= expected_task_revision
        && *mailbox_task_revision <= expected_task_revision
        && agent_id == mailbox_agent_id
        && agent_revision == mailbox_agent_revision
        && activity_id == mailbox_activity_id
        && turn_id == mailbox_turn_id
        && tool_call_id == mailbox_tool_call_id
        && mailbox_request.id == tool_call_id.as_str()
        && *mailbox_task_revision == binding.task_revision
        && binding.task_id == task_id.as_str()
        && binding.subagent_id == agent_id.as_str()
}

fn detached_mailbox_tool_call_matches(
    mailbox_request: &crate::runtime_permission::RuntimePermissionRequest,
    surface_tool_call_id: &surface::SurfaceToolCallId,
) -> bool {
    match &mailbox_request.context {
        crate::runtime_permission::RuntimePermissionContext::Child { tool_call_id, .. } => {
            tool_call_id == surface_tool_call_id && mailbox_request.id == tool_call_id.as_str()
        }
        crate::runtime_permission::RuntimePermissionContext::Foreground { .. } => false,
    }
}

fn is_detached_child_permission_interaction(
    snapshot: &surface::SurfaceSnapshot,
    interaction: &ResidentSurfaceInteraction,
) -> bool {
    let surface::SurfaceInteractionRequest::PermissionRequest { context, .. } =
        &interaction.record.request
    else {
        return false;
    };
    let surface::SurfacePermissionOwnerRef::Child { agent_id, .. } = &context.owner else {
        return false;
    };
    snapshot
        .subagents
        .iter()
        .find(|subagent| subagent.subagent_id == *agent_id)
        .is_some_and(|subagent| {
            matches!(
                &subagent.owner,
                surface::SurfaceSubagentOwner::DetachedTask { .. }
            )
        })
}

#[cfg(test)]
mod detached_permission_recovery_tests {
    use super::*;

    #[test]
    fn resolved_receipt_rehydrates_exact_permission_profile() {
        let profile = surface::SurfacePermissionProfile {
            file_system: Some(surface::SurfaceFileSystemPermissionProfile {
                read: Some(vec![surface::SurfacePermissionPathLabel(
                    surface::DisplayText::new("/workspace/src"),
                )]),
                write: None,
            }),
            network: Some(surface::SurfacePermissionNetworkProfile {
                enabled: Some(false),
                domains: vec![(
                    surface::SurfacePermissionDomainPattern(surface::DisplayText::new(
                        "api.example.com",
                    )),
                    surface::SurfaceAllowDeny::Deny,
                )],
            }),
        };
        let receipt = surface::SurfaceInteractionResolutionReceipt {
            response_id: surface::SurfaceResponseId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            receipt_id: surface::SurfaceResponseReceiptId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap(),
            kind: surface::SurfaceInteractionKind::PermissionRequest,
            safe_projection: surface::SurfaceInteractionSafeProjection::PermissionRequest {
                decision: surface::SurfaceAllowDeny::Allow,
                scope: surface::PermissionGrantScope::Turn,
                permissions: profile.clone(),
                strict_auto_review: true,
            },
        };
        let response = detached_permission_response_from_receipt(&receipt).unwrap();
        assert_eq!(
            response.decision,
            crate::protocol::PermissionResponseDecision::Allow
        );
        assert_eq!(response.scope, crate::protocol::PermissionGrantScope::Turn);
        assert!(response.strict_auto_review);
        assert_eq!(
            response.permissions,
            runtime_permission_profile_from_surface(&profile)
        );
    }
}
