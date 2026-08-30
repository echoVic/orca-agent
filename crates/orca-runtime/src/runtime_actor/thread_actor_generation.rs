// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

fn subagent_activity_projection(
    payload: &SubagentActivityPayload,
) -> (surface::DisplayText, Option<u32>, Option<UsageTotals>) {
    match payload {
        SubagentActivityPayload::Started { description } => (description.clone(), None, None),
        SubagentActivityPayload::PhaseChanged { phase, turn } => (
            surface::DisplayText::new(format!("phase: {phase:?}")),
            *turn,
            None,
        ),
        SubagentActivityPayload::ToolStarted { name, target, .. } => (
            target
                .as_ref()
                .map(|target| surface::DisplayText::new(format!("{name}: {}", target.as_str())))
                .unwrap_or_else(|| surface::DisplayText::new(name)),
            None,
            None,
        ),
        SubagentActivityPayload::ToolCompleted {
            status, summary, ..
        } => (
            summary.clone().unwrap_or_else(|| {
                surface::DisplayText::new(format!("tool completed: {status:?}"))
            }),
            None,
            None,
        ),
        SubagentActivityPayload::Usage { totals } => (
            surface::DisplayText::new("usage updated"),
            None,
            Some(totals.clone()),
        ),
        SubagentActivityPayload::CheckpointPublished {
            checkpoint_revision,
        } => (
            surface::DisplayText::new(format!("checkpoint {checkpoint_revision}")),
            None,
            None,
        ),
        SubagentActivityPayload::Completed { .. } => {
            (surface::DisplayText::new("completed"), None, None)
        }
    }
}

fn task_transcript_not_found(
    request_id: surface::SurfaceRequestId,
) -> surface::SurfaceReadResult<surface::TaskTranscriptSnapshot> {
    surface::SurfaceReadResult::NotFound {
        request_id,
        error: surface::SurfaceReadError {
            class: surface::SurfaceReadErrorClass::NotFound,
            code: surface::SurfaceReadErrorCode::NotFound,
            message: surface::DisplayText::new("task transcript was not found"),
            current_revision: None,
        },
    }
}

fn task_transcript_binding_error(
    request_id: surface::SurfaceRequestId,
    current_revision: Option<surface::SurfaceReadRevision>,
) -> surface::SurfaceReadResult<surface::TaskTranscriptSnapshot> {
    surface::SurfaceReadResult::Invalid {
        request_id,
        error: surface::SurfaceReadError {
            class: surface::SurfaceReadErrorClass::Invalid,
            code: surface::SurfaceReadErrorCode::BindingMismatch,
            message: surface::DisplayText::new("task transcript binding is invalid"),
            current_revision,
        },
    }
}

fn task_transcript_unavailable(
    request_id: surface::SurfaceRequestId,
) -> surface::SurfaceReadResult<surface::TaskTranscriptSnapshot> {
    surface::SurfaceReadResult::Unavailable {
        request_id,
        error: surface::SurfaceReadError {
            class: surface::SurfaceReadErrorClass::Unavailable,
            code: surface::SurfaceReadErrorCode::StoreUnavailable,
            message: surface::DisplayText::new("task transcript has no safe durable checkpoint"),
            current_revision: None,
        },
    }
}

fn surface_task_transcript_item(
    item: crate::agent_continuation::ChildTranscriptItem,
) -> Result<surface::TaskTranscriptItem, ()> {
    let item = match item {
        crate::agent_continuation::ChildTranscriptItem::User { content } => {
            surface::TaskTranscriptItem::User {
                content: surface_persisted_display_text(&content),
            }
        }
        crate::agent_continuation::ChildTranscriptItem::Assistant { content } => {
            surface::TaskTranscriptItem::Assistant {
                content: surface_persisted_display_text(&content),
            }
        }
        crate::agent_continuation::ChildTranscriptItem::ToolCall { id, name } => {
            surface::TaskTranscriptItem::ToolCall {
                id: surface::SurfaceHistoryId::try_new(id).map_err(|_| ())?,
                name: surface::NonEmptyText::try_new(name).map_err(|_| ())?,
            }
        }
        crate::agent_continuation::ChildTranscriptItem::ToolResult {
            id,
            content,
            status,
        } => surface::TaskTranscriptItem::ToolResult {
            id: surface::SurfaceHistoryId::try_new(id).map_err(|_| ())?,
            content: surface_persisted_display_text(&content),
            status: match status {
                orca_core::tool_types::ToolStatus::Completed => {
                    surface::TaskTranscriptToolStatus::Completed
                }
                orca_core::tool_types::ToolStatus::Failed => {
                    surface::TaskTranscriptToolStatus::Failed
                }
                orca_core::tool_types::ToolStatus::Denied => {
                    surface::TaskTranscriptToolStatus::Denied
                }
                orca_core::tool_types::ToolStatus::NotImplemented => {
                    surface::TaskTranscriptToolStatus::NotImplemented
                }
                orca_core::tool_types::ToolStatus::Cancelled => {
                    surface::TaskTranscriptToolStatus::Cancelled
                }
                orca_core::tool_types::ToolStatus::Indeterminate => {
                    surface::TaskTranscriptToolStatus::Indeterminate
                }
            },
        },
    };
    Ok(item)
}

fn task_transcript_record_matches_surface_task(
    task_id: &surface::SurfaceTaskId,
    task: &surface::SurfaceTask,
    record: &crate::tasks::TaskTranscriptRecord,
) -> bool {
    // The surface task is the authority for the public fence.  The registry
    // publication revision is a separate repairable-mirror counter and is
    // intentionally not part of this comparison.
    task.task_id == *task_id
        && record.task_id == task_id.as_str()
        && record.parent_task_id.as_deref()
            == task.parent_task_id.as_ref().map(|parent| parent.as_str())
}

impl ThreadActor {
    pub(super) fn admits_surface_client(
        &self,
        client: &surface::RuntimeSurfaceClientHandle,
        capability: surface::SurfaceCapability,
    ) -> bool {
        self.resident_surface.0.as_ref().is_some_and(|resident| {
            let admitted = resident.hub.admits_client(client);
            let capability_granted = client.grant().capabilities.as_set().contains(&capability);
            admitted && capability_granted
        })
    }

    pub(super) fn read_surface_task_transcript(
        &self,
        request_id: surface::SurfaceRequestId,
        task_id: surface::SurfaceTaskId,
        expected_revision: surface::TaskRevision,
    ) -> Result<
        surface::SurfaceReadResult<surface::TaskTranscriptSnapshot>,
        surface::SurfaceClientCommandError,
    > {
        let Some(resident) = self.resident_surface.0.as_ref() else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let snapshot = resident.coordinator.state().snapshot();
        let current_task = snapshot.tasks.iter().find(|task| task.task_id == task_id);
        let Some(current_task) = current_task else {
            return Ok(task_transcript_not_found(request_id));
        };
        let current_revision = surface::SurfaceReadRevision::Task {
            task_id: current_task.task_id.clone(),
            revision: current_task.revision,
        };
        if current_task.revision != expected_revision {
            return Ok(surface::SurfaceReadResult::Stale {
                request_id,
                error: surface::SurfaceReadError {
                    class: surface::SurfaceReadErrorClass::Stale,
                    code: surface::SurfaceReadErrorCode::StaleRevision,
                    message: surface::DisplayText::new("task transcript revision is stale"),
                    current_revision: Some(current_revision),
                },
            });
        }
        if current_task.task_type != surface::SurfaceTaskType::Subagent {
            return Ok(task_transcript_binding_error(
                request_id,
                Some(current_revision),
            ));
        }

        let Some(state) = self.state.as_ref() else {
            return Ok(task_transcript_unavailable(request_id));
        };
        let record = match state
            .thread
            .session()
            .task_registry()
            .read_task_transcript(task_id.as_str())
        {
            Ok(record) => record,
            Err(crate::tasks::TaskTranscriptReadError::NotFound) => {
                return Ok(task_transcript_not_found(request_id));
            }
            Err(crate::tasks::TaskTranscriptReadError::BindingMismatch) => {
                return Ok(task_transcript_binding_error(
                    request_id,
                    Some(current_revision),
                ));
            }
            Err(
                crate::tasks::TaskTranscriptReadError::Unavailable
                | crate::tasks::TaskTranscriptReadError::Corrupt,
            ) => return Ok(task_transcript_unavailable(request_id)),
        };
        if !task_transcript_record_matches_surface_task(&task_id, current_task, &record) {
            return Ok(task_transcript_binding_error(
                request_id,
                Some(current_revision),
            ));
        }
        let items = match record
            .items
            .into_iter()
            .map(surface_task_transcript_item)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(items) => items,
            Err(()) => return Ok(task_transcript_unavailable(request_id)),
        };

        Ok(surface::SurfaceReadResult::Found {
            request_id,
            revision: current_revision,
            value: surface::TaskTranscriptSnapshot {
                task_id,
                task_revision: expected_revision,
                checkpoint_revision: record.checkpoint_revision,
                turn: record.turn,
                usage: record.usage,
                complete: record.complete,
                items,
            },
        })
    }

    pub(super) fn surface_operation_batch(
        &self,
        operation_id: &surface::SurfaceOperationId,
        patches: Vec<surface::OperationPatch>,
    ) -> surface::SurfaceCommitBatch {
        self.surface_operation_batch_with_commit_id(operation_id, patches, None)
    }

    pub(super) fn surface_operation_batch_with_commit_id(
        &self,
        operation_id: &surface::SurfaceOperationId,
        patches: Vec<surface::OperationPatch>,
        commit_id: Option<surface::SurfaceCommitId>,
    ) -> surface::SurfaceCommitBatch {
        let generation_scope = patches.iter().find_map(|patch| match patch {
            surface::OperationPatch::GenerationReserved { generation } => {
                Some(generation.fence.clone())
            }
            surface::OperationPatch::GenerationStarted { fence, .. }
            | surface::OperationPatch::InputBindingsResolved { fence, .. }
            | surface::OperationPatch::InputBindingsFailed { fence, .. }
            | surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn { fence, .. },
            }
            | surface::OperationPatch::ModelRouteSelected { fence, .. }
            | surface::OperationPatch::GenerationStopped { fence, .. }
            | surface::OperationPatch::GenerationTransferred { fence, .. } => Some(fence.clone()),
            _ => None,
        });
        let events = patches
            .into_iter()
            .map(|patch| {
                let scope = match &patch {
                    surface::OperationPatch::GenerationReserved { generation } => {
                        surface::SurfaceScope::Generation {
                            fence: generation.fence.clone(),
                        }
                    }
                    surface::OperationPatch::GenerationStarted { fence, .. }
                    | surface::OperationPatch::InputBindingsResolved { fence, .. }
                    | surface::OperationPatch::InputBindingsFailed { fence, .. }
                    | surface::OperationPatch::ModelRouteSelected { fence, .. }
                    | surface::OperationPatch::GenerationStopped { fence, .. }
                    | surface::OperationPatch::GenerationTransferred { fence, .. } => {
                        surface::SurfaceScope::Generation {
                            fence: fence.clone(),
                        }
                    }
                    surface::OperationPatch::AgentLoopTurnStarted { turn } => {
                        surface::SurfaceScope::Generation {
                            fence: turn.fence.clone(),
                        }
                    }
                    surface::OperationPatch::FinalizationStarted { .. }
                        if generation_scope.is_some() =>
                    {
                        surface::SurfaceScope::Generation {
                            fence: generation_scope.clone().unwrap(),
                        }
                    }
                    _ => surface::SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                };
                (scope, surface::SurfaceEvent::Operation(patch))
            })
            .collect();
        self.surface_event_batch_with_commit_id(events, commit_id)
    }

    pub(super) fn surface_event_batch_with_commit_id(
        &self,
        events: Vec<(surface::SurfaceScope, surface::SurfaceEvent)>,
        commit_id: Option<surface::SurfaceCommitId>,
    ) -> surface::SurfaceCommitBatch {
        runtime_surface_event_batch(
            self.resident_surface.coordinator.state().snapshot(),
            events,
            commit_id,
        )
    }

    pub(super) fn commit_surface_generation_batch_with_retry(
        &mut self,
        fence: surface::SurfaceOperationFence,
        batch: &surface::SurfaceCommitBatch,
    ) -> io::Result<()> {
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence.clone(), batch)
            {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "failed to commit provider semantic batch: {error:?}"
                    )));
                }
            }
        }
        Err(io::Error::other(
            "provider semantic batch did not commit after bounded retries",
        ))
    }

    pub(super) fn prepare_queued_jsonl_resume(
        &mut self,
        active: &ActiveOperation,
        usage_delta: UsageTotals,
    ) -> Result<QueuedJsonlResumePreparation, RuntimeHostError> {
        let interrupted = active.surface_operation.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "queued JSONL resume lost its interrupted generation fence".to_string(),
            }
        })?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == interrupted.operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "queued JSONL resume lost its foreground operation".to_string(),
            })?;
        if !matches!(
            &operation.pending_control,
            Some(surface::PendingControlIntent::ResumeAfterInterruptedStop {
                generation_fence,
            }) if generation_fence == &interrupted
        ) {
            return Err(RuntimeHostError::ThreadStartFailed {
                message: "queued JSONL resume lacks its durable control intent".to_string(),
            });
        }
        let generation_id = surface::SurfaceGenerationId::new(
            interrupted
                .generation_id
                .get()
                .checked_add(1)
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "queued JSONL resume generation id exhausted".to_string(),
                })?,
        );
        let successor_fence = surface::SurfaceOperationFence {
            thread_id: interrupted.thread_id.clone(),
            thread_owner_epoch: interrupted.thread_owner_epoch,
            operation_id: interrupted.operation_id.clone(),
            generation_id,
        };
        let previous = operation
            .generations
            .last()
            .filter(|generation| generation.fence == interrupted)
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "queued JSONL resume predecessor disappeared".to_string(),
            })?;
        let successor = surface::GenerationRecord {
            fence: successor_fence.clone(),
            logical_turn_id: previous.logical_turn_id.clone(),
            input: previous.input.clone(),
            predecessor: Some(interrupted.clone()),
            attempt: surface::GenerationAttempt::RecoveryReplacement,
            goal_identity: None,
            replayability: previous.replayability.clone(),
            required_capabilities: previous.required_capabilities.clone(),
            capability_fingerprint: previous.capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let generation_scope = surface::SurfaceScope::Generation {
            fence: interrupted.clone(),
        };
        let mut suspend_events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == interrupted
                    && stream.state == surface::SurfaceAssistantStreamState::Open
            })
            .map(|stream| {
                (
                    generation_scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: surface::AssistantDiscardReason::GenerationInterrupted,
                    }),
                )
            })
            .collect::<Vec<_>>();
        suspend_events.extend([
            (
                generation_scope,
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                    fence: interrupted.clone(),
                    reason: surface::GenerationStopReason::InterruptedResumable,
                    usage_delta: surface_usage_totals(usage_delta),
                }),
            ),
            (
                surface::SurfaceScope::Operation {
                    operation_id: interrupted.operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::Suspended {
                    operation_id: interrupted.operation_id.clone(),
                    cause: surface::SuspensionCause::Interrupted {
                        generation_id: interrupted.generation_id,
                    },
                }),
            ),
            (
                surface::SurfaceScope::Generation {
                    fence: successor_fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationReserved {
                    generation: successor.clone(),
                }),
            ),
            (
                surface::SurfaceScope::Operation {
                    operation_id: interrupted.operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::ControlIntentCommitted {
                    operation_id: interrupted.operation_id.clone(),
                    request_id: operation.request_id.clone(),
                    intent: surface::PendingControlIntent::ResumeStarting {
                        generation_fence: successor_fence.clone(),
                    },
                }),
            ),
        ]);
        let suspend_batch = self.surface_event_batch_with_commit_id(suspend_events, None);
        self.resident_surface
            .coordinator
            .commit_live_generation_suspend_batch(interrupted.clone(), &suspend_batch)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("queued JSONL resume suspension commit failed: {error:?}"),
            })?;

        let started_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &interrupted.operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: successor_fence.clone(),
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
        if let Err(error) =
            self.commit_surface_generation_batch_with_retry(successor_fence.clone(), &started_batch)
        {
            let message =
                format!("queued JSONL resume failed after durable successor reservation: {error}");
            let _ = self.repair_surface_resume_failure(
                &successor_fence,
                "queued JSONL resume start repair",
            );
            self.operation_recovery.terminal_blocked = Some(message.clone());
            return Ok(QueuedJsonlResumePreparation::RecoveryRequired { message });
        }

        let task_id = format!("typed-jsonl-resume-{}", uuid::Uuid::now_v7());
        let loop_batch = self.surface_operation_batch(
            &interrupted.operation_id,
            vec![surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn {
                    turn_id: successor.logical_turn_id,
                    fence: successor_fence.clone(),
                    ordinal: 0,
                    task_id: surface::SurfaceTaskId::try_new(task_id.clone())
                        .expect("generated task id is non-empty"),
                    task_status: surface::SurfaceTaskRunningStatus::Running,
                },
            }],
        );
        if let Err(error) =
            self.commit_surface_generation_batch_with_retry(successor_fence.clone(), &loop_batch)
        {
            let message =
                format!("queued JSONL resume failed after durable successor start: {error}");
            let _ = self
                .repair_surface_resume_failure(&successor_fence, "queued JSONL resume loop repair");
            self.operation_recovery.terminal_blocked = Some(message.clone());
            return Ok(QueuedJsonlResumePreparation::RecoveryRequired { message });
        }
        Ok(QueuedJsonlResumePreparation::Started {
            successor_fence,
            task_id,
        })
    }

    pub(super) fn commit_surface_background_batch_with_retry(
        &mut self,
        fence: surface::SurfaceBackgroundFence,
        batch: &surface::SurfaceCommitBatch,
    ) -> io::Result<()> {
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self
                .resident_surface
                .coordinator
                .commit_background_batch(fence.clone(), batch)
            {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "failed to commit background provider semantic batch: {error:?}"
                    )));
                }
            }
        }
        Err(io::Error::other(
            "background provider semantic batch did not commit after bounded retries",
        ))
    }

    pub(super) fn commit_surface_actor_batch_with_retry(
        &mut self,
        batch: &surface::SurfaceCommitBatch,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        if self.operation_recovery.pending_manual_compaction.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self.resident_surface.coordinator.commit_actor_batch(batch) {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(_) => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            }
        }
        Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
    }

    pub(super) fn commit_surface_provider_steps(
        &mut self,
        active: Option<&ActiveOperation>,
        fence: surface::SurfaceOperationFence,
        identity: &orca_core::thread_item_projection::ModelResponseIdentity,
        steps: &[ProviderStep],
    ) -> io::Result<()> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let active_generation = active.is_some_and(|active| {
            active.surface_operation.as_ref() == Some(&fence)
                && !Self::surface_interaction_admission_closed(active)
        });
        let Some(projection) = self.generation_context_controller.provider_steps_events(
            &snapshot,
            active_generation,
            fence.clone(),
            identity,
            steps,
        )?
        else {
            return Ok(());
        };
        let batch = self.surface_event_batch_with_commit_id(projection.events, None);
        match projection.background_fence {
            Some(background_fence) => {
                self.commit_surface_background_batch_with_retry(background_fence, &batch)
            }
            None => self.commit_surface_generation_batch_with_retry(fence, &batch),
        }
    }

    pub(super) fn commit_surface_subagent_activity(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        event: SubagentActivityEvent,
    ) -> io::Result<()> {
        self.commit_subagent_activity_inner(
            Some(active),
            Some(&fence),
            &active.task_registry,
            event,
        )
    }

    /// Commits a detached child event after the parent generation has ended.
    /// The durable task binding, rather than a stale generation fence, is the
    /// authority for this path.
    pub(super) fn commit_detached_subagent_activity(
        &mut self,
        task_registry: &crate::tasks::TaskRegistry,
        event: SubagentActivityEvent,
    ) -> io::Result<()> {
        self.commit_subagent_activity_inner(None, None, task_registry, event)
    }

    fn commit_subagent_activity_inner(
        &mut self,
        active: Option<&ActiveOperation>,
        fence: Option<&surface::SurfaceOperationFence>,
        task_registry: &crate::tasks::TaskRegistry,
        event: SubagentActivityEvent,
    ) -> io::Result<()> {
        if let (Some(active), Some(fence)) = (active, fence)
            && (active.surface_operation.as_ref() != Some(fence)
                || Self::surface_interaction_admission_closed(active))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "subagent activity generation fence is stale or terminalizing",
            ));
        }
        if event.schema_version != SubagentActivityEvent::SCHEMA_VERSION || !event.verify_digest() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "subagent activity envelope failed schema or digest validation",
            ));
        }
        let detached_binding = match &event.owner {
            SubagentActivityOwner::Generation { operation_id } => {
                let Some(fence) = fence else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "generation-owned subagent activity requires an active generation",
                    ));
                };
                if operation_id != &fence.operation_id {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "subagent activity owner does not match the active generation",
                    ));
                }
                None
            }
            SubagentActivityOwner::DetachedTask {
                task_id,
                task_revision,
                authority_digest,
            } => {
                if task_id != &event.task_id {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "detached subagent activity task identity is invalid",
                    ));
                }
                let binding = task_registry
                    .detached_subagent_binding(event.task_id.as_str())
                    .map_err(io::Error::other)?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "detached subagent owner binding is missing or stale",
                        )
                    })?;
                if binding.subagent_id != event.subagent_id.as_str()
                    || binding.task_revision != *task_revision
                    || binding.attempt_id != event.attempt_id
                    || binding.authority_digest != *authority_digest
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "detached subagent activity owner binding is invalid",
                    ));
                }
                Some(binding)
            }
        };
        let surface_attempt_id = surface::SurfaceTaskAttemptId::try_new(event.attempt_id.as_str())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid subagent attempt identity",
                )
            })?;
        let projection_owner = match (&event.owner, fence, detached_binding.as_ref()) {
            (SubagentActivityOwner::Generation { .. }, Some(fence), _) => {
                surface::SurfaceSubagentOwner::Generation {
                    fence: fence.clone(),
                }
            }
            (SubagentActivityOwner::DetachedTask { .. }, _, Some(binding)) => {
                surface::SurfaceSubagentOwner::DetachedTask {
                    owner: surface::SurfaceTaskOwnerRef::new(
                        event.task_id.clone(),
                        binding.task_revision,
                        surface_attempt_id.clone(),
                        binding.authority_digest,
                    ),
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "subagent activity owner context is unavailable",
                ));
            }
        };

        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let durable_receipt = self
            .resident_surface
            .coordinator
            .lookup_commit(&event.surface_commit_id);
        if let Some(stored_source_digest) = self
            .resident_surface
            .coordinator
            .lookup_subagent_source_digest(&event.surface_commit_id)
        {
            if stored_source_digest != event.digest {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "subagent activity commit id was reused with a conflicting source digest",
                ));
            }
            // The surface ledger is durable and therefore takes precedence
            // over the bounded in-memory cache. A committed event can be
            // retried after a lost reply; its sequence is already reflected
            // in the projection, so acknowledge it without applying a second
            // patch. A future sequence under the same id is a conflict.
            let current_sequence = snapshot
                .subagents
                .iter()
                .find(|subagent| subagent.subagent_id == event.subagent_id)
                .map_or(0, |subagent| subagent.revision.get());
            if event.source_sequence <= current_sequence {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "subagent activity commit id was reused for a future sequence",
            ));
        }
        if durable_receipt.is_some()
            && self
                .resident_surface
                .coordinator
                .lookup_subagent_source_digest(&event.surface_commit_id)
                .is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "subagent activity commit id belongs to a different surface event",
            ));
        }

        // A command sender can lose the reply after the surface batch has
        // committed. Treat an exact retry as a no-op before validating the
        // next projection revision; otherwise the already-applied sequence
        // would be rejected as non-contiguous. Keep this cache bounded: the
        // durable surface ledger remains the recovery authority and detached
        // relay replay resumes from the projected revision after restart.
        if let Some((_, digest)) = self
            .subagent_activity_dedupe
            .iter()
            .find(|(commit_id, _)| *commit_id == event.surface_commit_id)
        {
            if *digest == event.digest {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "subagent activity commit id was reused with a conflicting digest",
            ));
        }

        let existing_subagent = snapshot
            .subagents
            .iter()
            .find(|subagent| subagent.subagent_id == event.subagent_id);
        let existing_task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == event.task_id);
        let (activity, turn, usage) = subagent_activity_projection(&event.payload);
        let terminal = match &event.payload {
            SubagentActivityPayload::Completed {
                status,
                output,
                error,
                usage,
            } => Some((status.clone(), output.clone(), error.clone(), usage.clone())),
            _ => None,
        };

        let (task_patch, subagent_patch) = match (existing_task, existing_subagent, terminal) {
            (None, None, None) if event.source_sequence == 1 => {
                let SubagentActivityPayload::Started { description } = &event.payload else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "first subagent activity must be started",
                    ));
                };
                let task = surface::SurfaceTask {
                    task_id: event.task_id.clone(),
                    revision: surface::TaskRevision::try_new(1)
                        .expect("one is a valid task revision"),
                    task_type: surface::SurfaceTaskType::Subagent,
                    status: surface::SurfaceTaskStatus::Running,
                    backgrounded: false,
                    description: description.clone(),
                    created_at: event.occurred_at,
                    started_at: Some(event.occurred_at),
                    completed_at: None,
                    parent_operation: fence.map(|fence| fence.operation_id.clone()).or_else(|| {
                        detached_binding.as_ref().and_then(|binding| {
                            binding
                                .parent_fence
                                .as_ref()
                                .map(|fence| fence.operation_id.clone())
                        })
                    }),
                    parent_task_id: task_registry
                        .get(event.task_id.as_str())
                        .and_then(|record| record.parent_task_id)
                        .and_then(|parent| surface::SurfaceTaskId::try_new(parent).ok()),
                    background_fence: None,
                    workflow_run_id: None,
                    subagent_id: Some(event.subagent_id.clone()),
                    pending_interaction_id: None,
                    usage: None,
                    result: None,
                    error: None,
                    retry_count: 0,
                    output_truncated: false,
                };
                let subagent = surface::RunningSurfaceSubagent::try_new(surface::SurfaceSubagent {
                    subagent_id: event.subagent_id.clone(),
                    task_id: event.task_id.clone(),
                    revision: surface::SubagentRevision::try_new(1)
                        .expect("one is a valid subagent revision"),
                    description: description.clone(),
                    status: surface::SurfaceSubagentStatus::Running,
                    activity: Some(description.clone()),
                    turn: None,
                    usage: None,
                    output: None,
                    error: None,
                    owner: projection_owner.clone(),
                    source: surface::SurfaceSubagentSource::new(
                        surface_attempt_id.clone(),
                        event.turn_id.clone(),
                        event.source_sequence,
                        event.surface_commit_id.clone(),
                        event.digest.clone(),
                    ),
                })
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid subagent start")
                })?;
                (
                    surface::TaskPatch::Upserted {
                        expected_revision: None,
                        task,
                    },
                    surface::SubagentPatch::Started {
                        expected_revision: surface::ExpectedAbsentSubagentRevision,
                        subagent,
                    },
                )
            }
            (Some(task), Some(subagent), terminal) => {
                if subagent.task_id != event.task_id || subagent.owner != projection_owner {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "subagent activity identity is not owned by this generation",
                    ));
                }
                let expected_sequence =
                    subagent.revision.get().checked_add(1).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "subagent revision overflow")
                    })?;
                if event.source_sequence != expected_sequence {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "subagent activity source sequence is not contiguous",
                    ));
                }
                let next_task_revision =
                    surface::TaskRevision::try_new(task.revision.get().checked_add(1).ok_or_else(
                        || io::Error::new(io::ErrorKind::InvalidData, "task revision overflow"),
                    )?)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "task revision overflow")
                    })?;
                let next_subagent_revision = surface::SubagentRevision::try_new(expected_sequence)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "subagent revision overflow")
                    })?;
                let mut next_task = task.clone();
                next_task.revision = next_task_revision;
                next_task.usage = usage
                    .clone()
                    .map(surface_usage_totals)
                    .or_else(|| task.usage.clone());
                let subagent_patch = match terminal {
                    Some((status, output, error, terminal_usage)) => {
                        next_task.status = match status {
                            surface::SurfaceSubagentTerminalStatus::Completed => {
                                surface::SurfaceTaskStatus::Completed
                            }
                            surface::SurfaceSubagentTerminalStatus::Failed => {
                                surface::SurfaceTaskStatus::Failed
                            }
                            surface::SurfaceSubagentTerminalStatus::Cancelled => {
                                surface::SurfaceTaskStatus::Cancelled
                            }
                        };
                        next_task.completed_at = Some(event.occurred_at);
                        next_task.result = output.clone();
                        next_task.error = error.clone();
                        next_task.usage = terminal_usage
                            .clone()
                            .map(surface_usage_totals)
                            .or(next_task.usage.clone());
                        surface::SubagentPatch::Completed {
                            subagent_id: event.subagent_id.clone(),
                            expected_revision: subagent.revision,
                            next_revision: next_subagent_revision,
                            owner: projection_owner.clone(),
                            source: surface::SurfaceSubagentSource::new(
                                surface_attempt_id.clone(),
                                event.turn_id.clone(),
                                event.source_sequence,
                                event.surface_commit_id.clone(),
                                event.digest.clone(),
                            ),
                            status,
                            output,
                            error,
                            usage: terminal_usage
                                .map(surface_usage_totals)
                                .or_else(|| subagent.usage.clone()),
                        }
                    }
                    None => surface::SubagentPatch::Progress {
                        subagent_id: event.subagent_id.clone(),
                        expected_revision: subagent.revision,
                        next_revision: next_subagent_revision,
                        owner: projection_owner.clone(),
                        source: surface::SurfaceSubagentSource::new(
                            surface_attempt_id.clone(),
                            event.turn_id.clone(),
                            event.source_sequence,
                            event.surface_commit_id.clone(),
                            event.digest.clone(),
                        ),
                        activity: activity.clone(),
                        turn: turn.or(subagent.turn),
                        usage: usage
                            .map(surface_usage_totals)
                            .or_else(|| subagent.usage.clone()),
                    },
                };
                (
                    surface::TaskPatch::Upserted {
                        expected_revision: Some(task.revision),
                        task: next_task,
                    },
                    subagent_patch,
                )
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "subagent activity start/progress state is inconsistent",
                ));
            }
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(task_patch),
                ),
                (
                    match &projection_owner {
                        surface::SurfaceSubagentOwner::Generation { fence } => {
                            surface::SurfaceScope::Generation {
                                fence: fence.clone(),
                            }
                        }
                        surface::SurfaceSubagentOwner::DetachedTask { .. } => {
                            surface::SurfaceScope::Thread
                        }
                    },
                    surface::SurfaceEvent::Subagent(subagent_patch),
                ),
            ],
            Some(event.surface_commit_id.clone()),
        );
        if let Some(receipt) = durable_receipt {
            let stored_batch_digest = match receipt {
                surface::SurfaceBatchReceipt::Recorded(receipt) => receipt.batch_digest,
                surface::SurfaceBatchReceipt::Ephemeral(receipt) => receipt.batch_digest,
            };
            if stored_batch_digest != batch.batch_digest {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "subagent activity commit id was reused with a conflicting batch digest",
                ));
            }
        }
        // Activity batches intentionally cross the Thread and Generation
        // scopes: the task cursor and the child projection must advance in
        // one durable commit.  Route through the actor-owned activity
        // authority rather than the generation-only publisher permit.
        self.commit_surface_actor_batch_with_retry(&batch)
            .map_err(|error| {
                io::Error::other(format!("failed to commit subagent activity: {error:?}"))
            })?;

        if self.subagent_activity_dedupe.len() >= super::SUBAGENT_ACTIVITY_DEDUPE_CAPACITY {
            self.subagent_activity_dedupe.pop_front();
        }
        self.subagent_activity_dedupe
            .push_back((event.surface_commit_id.clone(), event.digest.clone()));

        // This is deliberately a repairable latest-state mirror. The source
        // event reached the surface ledger before the registry is touched.
        let _ = task_registry.update_subagent_activity(
            event.task_id.as_str(),
            activity.as_str().to_string(),
            turn,
            usage,
        );
        Ok(())
    }

    /// Replays detached child events through the same actor-owned commit path.
    /// Relay frames are never acknowledged by mutating the task mirror; the
    /// surface ledger's commit id is the replay cursor and deduplication key.
    pub(super) fn drain_subagent_relay(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        task_id: &str,
        attempt_id: &str,
    ) -> io::Result<()> {
        let reader = active
            .task_registry
            .open_subagent_event_relay_reader(task_id, attempt_id)
            .map_err(io::Error::other)?;
        let mut after_sequence = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .subagents
            .iter()
            .find(|subagent| subagent.task_id.as_str() == task_id)
            .map_or(0, |subagent| subagent.revision.get());
        loop {
            let page = reader.read_page(after_sequence).map_err(io::Error::other)?;
            if page.records.is_empty() {
                return Ok(());
            }
            for record in &page.records {
                if record.task_id != task_id
                    || record.attempt_id != attempt_id
                    || record.source_sequence <= after_sequence
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "detached relay record identity or sequence is invalid",
                    ));
                }
                let event: SubagentActivityEvent =
                    serde_json::from_slice(&record.payload).map_err(io::Error::other)?;
                if event.surface_commit_id != record.surface_commit_id
                    || event.source_sequence != record.source_sequence
                    || event.task_id.as_str() != task_id
                    || event.attempt_id.as_str() != attempt_id
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "detached relay payload does not match its frame",
                    ));
                }
                if matches!(event.owner, SubagentActivityOwner::DetachedTask { .. }) {
                    self.commit_detached_subagent_activity(&active.task_registry, event)?;
                } else {
                    self.commit_surface_subagent_activity(active, fence.clone(), event)?;
                }
                after_sequence = record.source_sequence;
            }
            if !page.has_more {
                return Ok(());
            }
        }
    }

    /// Drains a relay whose parent generation is no longer resident.  The
    /// detached binding is checked by `commit_detached_subagent_activity` for
    /// every frame; no terminal generation fence is resurrected.
    pub(super) fn drain_detached_subagent_relay(
        &mut self,
        task_registry: &crate::tasks::TaskRegistry,
        binding: &crate::tasks::DetachedSubagentBinding,
    ) -> io::Result<()> {
        let reader = task_registry
            .open_subagent_event_relay_reader(&binding.task_id, binding.attempt_id.as_str())
            .map_err(io::Error::other)?;
        let mut after_sequence = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .subagents
            .iter()
            .find(|subagent| subagent.task_id.as_str() == binding.task_id)
            .map_or(0, |subagent| subagent.source.source_sequence);
        loop {
            let page = reader.read_page(after_sequence).map_err(io::Error::other)?;
            if page.records.is_empty() {
                return Ok(());
            }
            for record in &page.records {
                if record.task_id != binding.task_id
                    || record.attempt_id != binding.attempt_id.as_str()
                    || record.source_sequence <= after_sequence
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "detached relay record identity or sequence is invalid",
                    ));
                }
                let event: SubagentActivityEvent =
                    serde_json::from_slice(&record.payload).map_err(io::Error::other)?;
                if event.surface_commit_id != record.surface_commit_id
                    || event.source_sequence != record.source_sequence
                    || event.task_id.as_str() != binding.task_id
                    || event.attempt_id.as_str() != binding.attempt_id.as_str()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "detached relay payload does not match its frame",
                    ));
                }
                self.commit_detached_subagent_activity(task_registry, event)?;
                after_sequence = record.source_sequence;
            }
            if !page.has_more {
                return Ok(());
            }
        }
    }

    pub(super) fn commit_surface_provider_failure(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        identity: &orca_core::thread_item_projection::ModelResponseIdentity,
        message: &str,
    ) -> io::Result<()> {
        self.commit_surface_provider_attempt_failure(active, fence, identity, message)?;
        active.surface_execution_failure = Some(surface::GenerationExecutionFailureClass::Provider);
        active.surface_execution_failure_diagnostic = Some(surface_safe_diagnostic(
            message,
            "provider execution failed with an invalid diagnostic",
        ));
        Ok(())
    }

    pub(super) fn commit_surface_provider_attempt_failure(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        identity: &orca_core::thread_item_projection::ModelResponseIdentity,
        _message: &str,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider attempt failure generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let events = self
            .generation_context_controller
            .provider_attempt_failure_events(&snapshot, &fence, identity)?;
        if events.is_empty() {
            return Ok(());
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
    }

    pub(super) fn commit_surface_provider_response(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        response: &crate::model_response::RuntimeModelResponse,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider response generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let events = self
            .generation_context_controller
            .provider_response_events(&snapshot, &fence, response)?;
        if events.is_empty() {
            return Ok(());
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
    }

    pub(super) fn commit_surface_plan_update(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        update: &orca_core::plan_types::UpdatePlanArgs,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence)
            || Self::surface_interaction_admission_closed(active)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "plan update generation fence is stale or terminalizing",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        if !operation
            .generations
            .iter()
            .any(|generation| generation.fence == fence)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "surface generation missing",
            ));
        }
        let revision =
            snapshot.plan.revision.get().checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "plan revision overflow")
            })?;
        let items = update
            .plan
            .iter()
            .map(|item| {
                Ok(surface::SurfacePlanItem {
                    step: surface::NonEmptyText::try_new(item.step.clone()).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "plan step is empty")
                    })?,
                    priority: surface::SurfacePlanPriority::Medium,
                    status: match item.status {
                        orca_core::plan_types::PlanStatus::Pending => {
                            surface::SurfacePlanStatus::Pending
                        }
                        orca_core::plan_types::PlanStatus::InProgress => {
                            surface::SurfacePlanStatus::InProgress
                        }
                        orca_core::plan_types::PlanStatus::Completed => {
                            surface::SurfacePlanStatus::Completed
                        }
                    },
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let plan = surface::SurfacePlanSnapshot {
            revision: surface::PlanRevision::try_new(revision).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "plan revision is invalid")
            })?,
            explanation: update
                .explanation
                .as_deref()
                .map(surface_persisted_display_text),
            items,
            causative_generation: Some(fence.clone()),
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Plan(plan),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)
            .map_err(|_| io::Error::other("failed to commit plan update facts"))
    }

    pub(super) fn surface_completed_tool_result(
        tool: &surface::SurfaceToolView,
        result: &orca_core::tool_types::ToolResult,
    ) -> io::Result<(surface::SurfaceToolResult, surface::DisplayText)> {
        if tool.request.name.as_str() != result.name.as_str() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool completion name differs from the committed provider tool",
            ));
        }
        let kind = match result.kind {
            orca_core::tool_types::ToolResultKind::Success
            | orca_core::tool_types::ToolResultKind::Empty
            | orca_core::tool_types::ToolResultKind::NoMatches
            | orca_core::tool_types::ToolResultKind::Truncated => {
                surface::SurfaceToolResultKind::Success
            }
            orca_core::tool_types::ToolResultKind::PermissionDenied => {
                surface::SurfaceToolResultKind::Denied
            }
            orca_core::tool_types::ToolResultKind::InvalidInput => {
                surface::SurfaceToolResultKind::InvalidArguments
            }
            orca_core::tool_types::ToolResultKind::RuntimeError => {
                surface::SurfaceToolResultKind::Failed
            }
            orca_core::tool_types::ToolResultKind::Cancelled => {
                surface::SurfaceToolResultKind::Cancelled
            }
            orca_core::tool_types::ToolResultKind::Indeterminate => {
                surface::SurfaceToolResultKind::ExternalEffectAmbiguous
            }
        };
        let source = match result.source {
            orca_core::tool_types::ToolTerminalSource::Observed => {
                surface::ToolTerminalSource::Observed
            }
            orca_core::tool_types::ToolTerminalSource::CompatibilityRepair => {
                surface::ToolTerminalSource::CompatibilityRepair
            }
        };
        let invocation_started = match result.started {
            orca_core::tool_types::ToolInvocationStarted::Yes => {
                surface::ToolInvocationStarted::Yes
            }
            orca_core::tool_types::ToolInvocationStarted::No => surface::ToolInvocationStarted::No,
            orca_core::tool_types::ToolInvocationStarted::Unknown => {
                surface::ToolInvocationStarted::Unknown
            }
        };
        let terminal = surface::SurfaceToolTerminal {
            kind,
            source,
            invocation_started,
        };
        let output = result.output.as_deref().map(surface_persisted_display_text);
        let error = result.error.as_deref().map(surface_persisted_display_text);
        let content = output
            .clone()
            .or_else(|| error.clone())
            .unwrap_or_else(|| surface::DisplayText::new("(no output)"));
        let (output, error) = if output.is_none() && error.is_none() {
            if matches!(terminal.kind, surface::SurfaceToolResultKind::Success) {
                (Some(content.clone()), None)
            } else {
                (None, Some(content.clone()))
            }
        } else {
            (output, error)
        };
        Ok((
            surface::SurfaceToolResult {
                tool_call_id: tool.request.tool_call_id.clone(),
                name: tool.request.name.clone(),
                terminal,
                output,
                error,
                exit_code: if matches!(tool.request.action, surface::SurfaceToolAction::Shell) {
                    result.exit_code
                } else {
                    None
                },
                truncated: result.truncated,
                file_change: None,
            },
            content,
        ))
    }

    pub(super) fn commit_surface_tool_results(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        results: &[orca_core::tool_types::ToolResult],
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool result generation fence is stale",
            ));
        }
        if results.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tool completion batch is empty",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut seen = BTreeSet::new();
        let mut events = Vec::with_capacity(results.len() * 2);
        for result in results {
            let tool_call_id = surface::SurfaceToolCallId::try_new(result.id.clone())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
            if !seen.insert(tool_call_id.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tool completion batch repeats a tool call id",
                ));
            }
            let tool = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == tool_call_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "tool completion lacks a committed provider tool identity",
                    )
                })?;
            if tool.request.turn_id != generation.logical_turn_id || tool.result.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tool completion differs from the active committed provider tool",
                ));
            }
            let (completed, content) = Self::surface_completed_tool_result(tool, result)?;
            let terminal = completed.terminal.clone();
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Tool(surface::ToolPatch::Completed { result: completed }),
            ));
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                    item: surface::SurfaceItem::ToolResultMessage {
                        id: surface::SurfaceItemId::new(),
                        turn_id: tool.request.turn_id.clone(),
                        tool_call_id,
                        content,
                        terminal,
                        pinned: false,
                    },
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
    }

    /// Function intent contract:
    ///
    /// - Input: a previously validated durable tool intent, its historical
    ///   generation fence, matching recovered execution fingerprint, approved
    ///   interaction receipt, and complete execution dependencies.
    /// - Output: dispatches through the normal tool execution stack while
    ///   skipping only the completed approval gate, then commits the result to
    ///   the historical generation.
    /// - Errors: rejects missing/mismatched projected tools, stale fences,
    ///   non-requested state, existing start receipts, or any start/result
    ///   commit failure before exposing success.
    /// - State changes and external calls: commits durable InvocationStarted
    ///   before tool dispatch and never creates or mutates `ActiveOperation`.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(super) fn dispatch_recovered_tool_intent<W: io::Write>(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        intent: &surface::ToolInvocationIntent,
        fence: &surface::SurfaceOperationFence,
        expected_execution_context_fingerprint: &surface::Sha256Digest,
        observed_execution_context_fingerprint: &surface::Sha256Digest,
        approval_receipt: &surface::SurfaceInteractionResolutionReceipt,
        dependencies: RecoveredToolExecutionDependencies<'_>,
        subagent_child_executor: crate::agent_child::ChildAgentExecutor<io::Sink>,
        workflow_child_executor: crate::agent_child::ChildAgentExecutor<
            crate::workflow::runner::SharedEventBuffer,
        >,
    ) -> io::Result<ToolExecutionCompletion> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *intent.invocation_id())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recovered tool intent lacks a committed provider tool",
                )
            })?;
        if tool.request != *intent.request()
            || tool.request.turn_id != generation.logical_turn_id
            || tool.state != surface::SurfaceToolViewState::Requested
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered tool intent does not match its requested historical projection",
            ));
        }
        let existing_started_receipt = tool.invocation_started.clone();
        let mut committer = RuntimeOwnedRecoveredToolCommitter { actor: self };
        execute_recovered_tool_intent(
            config,
            events,
            sink,
            intent,
            RecoveredToolInvocationAuthorization {
                fence,
                expected_execution_context_fingerprint,
                observed_execution_context_fingerprint,
                approval_receipt,
                existing_started_receipt: existing_started_receipt.as_ref(),
            },
            dependencies,
            subagent_child_executor,
            workflow_child_executor,
            &mut committer,
        )
    }

    /// Function intent contract:
    ///
    /// - Input: a validated durable pre-side-effect permission retry intent,
    ///   its historical fence/fingerprint, durable allow receipt and exact
    ///   permission response, plus rebuilt execution dependencies.
    /// - Output: re-dispatches the bound tool once without repeating approval,
    ///   consuming only the persisted permission answer.
    /// - Errors: rejects stale tools/fences, started projections, fingerprint
    ///   mismatch, non-allow receipts, or start/result commit failures.
    /// - State changes and external calls: commits `InvocationStarted` before
    ///   hooks/router/tool execution and never recreates an active operation.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_recovered_permission_retry<W: io::Write>(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        intent: &surface::PermissionRetryIntent,
        fence: &surface::SurfaceOperationFence,
        expected_execution_context_fingerprint: &surface::Sha256Digest,
        observed_execution_context_fingerprint: &surface::Sha256Digest,
        permission_receipt: &surface::SurfaceInteractionResolutionReceipt,
        permission_response: &crate::runtime_permission::RuntimePermissionResponse,
        dependencies: RecoveredToolExecutionDependencies<'_>,
        subagent_child_executor: crate::agent_child::ChildAgentExecutor<io::Sink>,
        workflow_child_executor: crate::agent_child::ChildAgentExecutor<
            crate::workflow::runner::SharedEventBuffer,
        >,
    ) -> io::Result<ToolExecutionCompletion> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *intent.invocation_id())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recovered permission intent lacks a committed provider tool",
                )
            })?;
        if tool.request != *intent.tool()
            || tool.request.turn_id != generation.logical_turn_id
            || tool.state != surface::SurfaceToolViewState::Requested
            || tool.result.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered permission intent does not match its not-started historical tool",
            ));
        }
        let existing_started_receipt = tool.invocation_started.clone();
        let mut committer = RuntimeOwnedRecoveredToolCommitter { actor: self };
        execute_recovered_permission_retry_intent(
            config,
            events,
            sink,
            intent,
            RecoveredPermissionRetryAuthorization {
                fence,
                expected_execution_context_fingerprint,
                observed_execution_context_fingerprint,
                permission_receipt,
                permission_response,
                existing_started_receipt: existing_started_receipt.as_ref(),
            },
            dependencies,
            subagent_child_executor,
            workflow_child_executor,
            &mut committer,
        )
    }

    /// Function intent contract:
    ///
    /// - Input: a cold-recovery authority bound to one historical generation,
    ///   invocation id, durable `InvocationStarted` receipt revision, and one
    ///   completed observed tool result.
    /// - Output: commits only that tool terminal result and its paired durable
    ///   result item; an identical already-committed result succeeds
    ///   idempotently.
    /// - Errors: rejects stale fences/revisions, wrong invocation identities,
    ///   missing start receipts, non-observed/non-started terminals, and
    ///   conflicting repeated results without changing durable state.
    /// - State changes and external calls: may append one recorded historical
    ///   result batch; it does not create or mutate `ActiveOperation`, dispatch
    ///   a tool, or replay a provider response.
    #[allow(dead_code)]
    pub(super) fn commit_recovery_authorized_historical_tool_result(
        &mut self,
        authority: &surface::HistoricalToolResultCommitAuthority,
        result: &orca_core::tool_types::ToolResult,
    ) -> io::Result<()> {
        let fence = authority.historical_fence();
        let invocation_id = authority.invocation_id();
        let result_invocation_id = surface::SurfaceToolCallId::try_new(result.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
        if &result_invocation_id != invocation_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered tool result invocation id differs from its authority",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *invocation_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recovered tool completion lacks a committed provider tool identity",
                )
            })?;
        let receipt_matches = tool.invocation_started.as_ref().is_some_and(|receipt| {
            receipt.invocation_id() == invocation_id
                && receipt.fence() == fence
                && receipt.revision() == authority.expected_projection_revision()
        });
        if tool.request.turn_id != generation.logical_turn_id || !receipt_matches {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered tool completion has a stale fence or invocation projection revision",
            ));
        }
        let (completed, content) = Self::surface_completed_tool_result(tool, result)?;
        if !matches!(
            &completed.terminal,
            surface::SurfaceToolTerminal {
                source: surface::ToolTerminalSource::Observed,
                invocation_started: surface::ToolInvocationStarted::Yes,
                ..
            }
        ) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered tool completion must be observed after a durable invocation start",
            ));
        }
        if let Some(existing) = &tool.result {
            return if existing == &completed {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recovered tool completion conflicts with the durable terminal result",
                ))
            };
        }
        if tool.state != surface::SurfaceToolViewState::Running {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovered tool completion is stale for the projected invocation state",
            ));
        }
        let terminal = completed.terminal.clone();
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    scope.clone(),
                    surface::SurfaceEvent::Tool(surface::ToolPatch::Completed {
                        result: completed,
                    }),
                ),
                (
                    scope,
                    surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                        item: surface::SurfaceItem::ToolResultMessage {
                            id: surface::SurfaceItemId::new(),
                            turn_id: tool.request.turn_id.clone(),
                            tool_call_id: invocation_id.clone(),
                            content,
                            terminal,
                            pinned: false,
                        },
                    }),
                ),
            ],
            None,
        );
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self
                .resident_surface
                .coordinator
                .commit_historical_tool_result_batch(authority, &batch)
            {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "failed to commit recovered historical tool result: {error:?}"
                    )));
                }
            }
        }
        Err(io::Error::other(
            "recovered historical tool result did not commit after bounded retries",
        ))
    }

    pub(super) fn commit_surface_workflow_started(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        started: &surface::RuntimeWorkflowStarted,
    ) -> io::Result<surface::RuntimeWorkflowIngressReceipt> {
        if active.surface_operation.as_ref() != Some(&fence)
            || Self::surface_interaction_admission_closed(active)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow start generation fence is stale or terminalizing",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == started.tool_call_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workflow start lacks a committed provider tool identity",
                )
            })?;
        if tool.request.turn_id != generation.logical_turn_id
            || !matches!(
                tool.request.name.as_str(),
                "Workflow" | "WorkflowDraftAction"
            )
            || tool.result.is_some()
            || snapshot
                .tasks
                .iter()
                .any(|task| task.task_id == started.task_id)
            || snapshot
                .workflows
                .iter()
                .any(|workflow| workflow.workflow_run_id == started.workflow_run_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow start differs from the active committed provider tool",
            ));
        }

        let task_revision = surface::TaskRevision::try_new(1).expect("one is valid");
        let workflow_revision = surface::WorkflowRevision::try_new(1).expect("one is valid");
        let task = surface::SurfaceTask {
            task_id: started.task_id.clone(),
            revision: task_revision,
            task_type: surface::SurfaceTaskType::Workflow,
            status: surface::SurfaceTaskStatus::Running,
            backgrounded: false,
            description: surface::DisplayText::new(started.name.as_str()),
            created_at: started.created_at,
            started_at: Some(started.created_at),
            completed_at: None,
            parent_operation: Some(fence.operation_id.clone()),
            parent_task_id: None,
            background_fence: None,
            workflow_run_id: Some(started.workflow_run_id.clone()),
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        };
        let workflow = surface::SurfaceWorkflow {
            workflow_run_id: started.workflow_run_id.clone(),
            task_id: started.task_id.clone(),
            revision: workflow_revision,
            name: started.name.clone(),
            status: surface::SurfaceWorkflowStatus::Running,
            phases: started
                .phases
                .iter()
                .cloned()
                .map(|name| surface::SurfaceWorkflowPhase {
                    name,
                    status: surface::SurfaceWorkflowStatus::Queued,
                    started_at: None,
                    completed_at: None,
                    agent_count: 0,
                    summary: None,
                    error: None,
                })
                .collect(),
            agents: Vec::new(),
            result: None,
            error: None,
            parent: Some(fence.clone()),
        };
        let receipt = surface::RuntimeWorkflowIngressReceipt {
            workflow: surface::SurfaceWorkflowFence {
                workflow_run_id: started.workflow_run_id.clone(),
                workflow_revision,
                parent: Some(fence.clone()),
            },
            task: surface::SurfaceTaskFence {
                task_id: started.task_id.clone(),
                task_revision,
                background_owner: None,
            },
            tool_call_id: started.tool_call_id.clone(),
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::Upserted {
                        expected_revision: None,
                        task,
                    }),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Workflow(surface::WorkflowPatch::Started { workflow }),
                ),
            ],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)
            .map_err(|_| io::Error::other("failed to commit workflow start facts"))?;
        Ok(receipt)
    }

    pub(super) fn commit_surface_workflow_finished(
        &mut self,
        active: &ActiveOperation,
        fence: surface::SurfaceOperationFence,
        finished: &surface::RuntimeWorkflowFinished,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow finish generation fence is stale",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == finished.receipt.tool_call_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workflow finish lacks its committed provider tool identity",
                )
            })?;
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == finished.receipt.task.task_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "surface workflow task missing")
            })?;
        let workflow = snapshot
            .workflows
            .iter()
            .find(|workflow| workflow.workflow_run_id == finished.receipt.workflow.workflow_run_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface workflow missing"))?;
        if tool.request.turn_id != generation.logical_turn_id
            || !matches!(
                tool.request.name.as_str(),
                "Workflow" | "WorkflowDraftAction"
            )
            || finished.receipt.workflow.parent.as_ref() != Some(&fence)
            || workflow.parent.as_ref() != Some(&fence)
            || workflow.revision != finished.receipt.workflow.workflow_revision
            || task.revision != finished.receipt.task.task_revision
            || task.parent_operation.as_ref() != Some(&fence.operation_id)
            || task.workflow_run_id.as_ref() != Some(&workflow.workflow_run_id)
            || workflow.task_id != task.task_id
            || task.status != surface::SurfaceTaskStatus::Running
            || workflow.status != surface::SurfaceWorkflowStatus::Running
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow finish receipt is stale or belongs to another generation",
            ));
        }

        let next_task_revision = surface::TaskRevision::try_new(
            task.revision
                .get()
                .checked_add(1)
                .ok_or_else(|| io::Error::other("workflow task revision exhausted"))?,
        )
        .map_err(|_| io::Error::other("workflow task revision is invalid"))?;
        let terminal_workflow_revision = surface::WorkflowRevision::try_new(
            workflow
                .revision
                .get()
                .checked_add(1)
                .ok_or_else(|| io::Error::other("workflow revision exhausted"))?,
        )
        .map_err(|_| io::Error::other("workflow revision is invalid"))?;
        let result_workflow_revision = surface::WorkflowRevision::try_new(
            terminal_workflow_revision
                .get()
                .checked_add(1)
                .ok_or_else(|| io::Error::other("workflow result revision exhausted"))?,
        )
        .map_err(|_| io::Error::other("workflow result revision is invalid"))?;
        let workflow_fence = finished.receipt.workflow.clone();
        let (task_status, task_result, task_error, workflow_terminal, result_status, content) =
            match &finished.outcome {
                surface::RuntimeWorkflowOutcome::Completed { status_line } => (
                    surface::SurfaceTaskStatus::Completed,
                    Some(status_line.clone()),
                    None,
                    surface::WorkflowPatch::Completed {
                        fence: workflow_fence.clone(),
                        next_revision: terminal_workflow_revision,
                    },
                    surface::SurfaceWorkflowResultStatus::Success,
                    status_line.clone(),
                ),
                surface::RuntimeWorkflowOutcome::Failed { error } => (
                    surface::SurfaceTaskStatus::Failed,
                    None,
                    Some(error.clone()),
                    surface::WorkflowPatch::Failed {
                        fence: workflow_fence.clone(),
                        next_revision: terminal_workflow_revision,
                        error: error.clone(),
                    },
                    surface::SurfaceWorkflowResultStatus::Failed,
                    error.clone(),
                ),
                surface::RuntimeWorkflowOutcome::Cancelled { reason } => (
                    surface::SurfaceTaskStatus::Cancelled,
                    None,
                    Some(reason.clone()),
                    surface::WorkflowPatch::Cancelled {
                        fence: workflow_fence.clone(),
                        next_revision: terminal_workflow_revision,
                        reason: reason.clone(),
                    },
                    surface::SurfaceWorkflowResultStatus::Failed,
                    reason.clone(),
                ),
            };
        let result = surface::SurfaceWorkflowResult {
            result_id: surface::SurfaceWorkflowResultId::try_new(format!(
                "workflow-result-{}",
                workflow.workflow_run_id.as_str()
            ))
            .map_err(|_| io::Error::other("workflow result identity is invalid"))?,
            tool_use_id: Some(finished.receipt.tool_call_id.clone()),
            status: result_status,
            content,
            acknowledged_by_operation: None,
        };
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                        task_id: task.task_id.clone(),
                        expected_revision: task.revision,
                        next_revision: next_task_revision,
                        status: task_status,
                        completed_at: Some(finished.completed_at),
                        result: task_result,
                        error: task_error,
                    }),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Workflow(workflow_terminal),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Workflow(surface::WorkflowPatch::ResultReady {
                        fence: surface::SurfaceWorkflowFence {
                            workflow_run_id: workflow.workflow_run_id.clone(),
                            workflow_revision: terminal_workflow_revision,
                            parent: Some(fence),
                        },
                        next_revision: result_workflow_revision,
                        result,
                    }),
                ),
            ],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)
            .map_err(|_| io::Error::other("failed to commit workflow completion facts"))
    }

    pub(super) fn committed_surface_mutation<T>(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Operation {
                    thread_id: batch.cursor_after.thread_id.clone(),
                    operation_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Operation,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("operation commit has one acknowledgement"),
            },
            value,
        }
    }

    pub(super) fn committed_surface_resume_mutation(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        generation: surface::SurfaceOperationFence,
        resume_batch: &surface::SurfaceCommitBatch,
        started_batch: &surface::SurfaceCommitBatch,
    ) -> surface::MutationReply<surface::ResumeOperationOutput> {
        let reserved_event = &resume_batch.events.as_slice()[0];
        let resume_event = &resume_batch.events.as_slice()[1];
        let started_event = &started_batch.events.as_slice()[0];
        let receipt =
            |role, event: &surface::SurfaceEventEnvelope, batch: &surface::SurfaceCommitBatch| {
                surface::ResumeTransitionReceipt {
                    role,
                    event_id: event.event_id.clone(),
                    cursor: batch.cursor_after.clone(),
                    commit_class: batch.commit_class.clone(),
                }
            };
        let resume_starting = receipt(
            surface::ResumeTransitionRole::ResumeStarting,
            resume_event,
            resume_batch,
        );
        let generation_reserved = receipt(
            surface::ResumeTransitionRole::GenerationReserved,
            reserved_event,
            resume_batch,
        );
        let generation_started = receipt(
            surface::ResumeTransitionRole::GenerationStarted,
            started_event,
            started_batch,
        );
        let acknowledgement = |receipt: &surface::ResumeTransitionReceipt| {
            surface::MutationCommitAck::ThreadLocalCursor {
                cursor: receipt.cursor.clone(),
                family: surface::SurfaceFactFamily::Operation,
                event_id: receipt.event_id.clone(),
                commit_class: receipt.commit_class.clone(),
            }
        };
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Operation {
                    thread_id: generation.thread_id.clone(),
                    operation_id: operation_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    acknowledgement(&resume_starting),
                    acknowledgement(&generation_reserved),
                    acknowledgement(&generation_started),
                ])
                .expect("resume commit has three acknowledgements"),
            },
            value: surface::ResumeOperationOutput {
                operation_id,
                generation,
                resume_starting,
                generation_reserved,
                generation_started,
                waiter: surface::OperationWaiterHandle::new(),
            },
        }
    }

    pub(super) fn committed_settings_mutation<T>(
        &self,
        request_id: surface::SurfaceRequestId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::RuntimeSettings {
                    host_incarnation: self
                        .resident_surface
                        .hub
                        .authority()
                        .host_incarnation()
                        .clone(),
                    thread_id: Some(batch.cursor_after.thread_id.clone()),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Settings,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("settings commit has one acknowledgement"),
            },
            value,
        }
    }

    pub(super) fn committed_pinned_context_mutation<T>(
        &self,
        request_id: surface::SurfaceRequestId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Thread {
                    thread_id: batch.cursor_after.thread_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::PinnedContext,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("pinned context commit has one acknowledgement"),
            },
            value,
        }
    }

    pub(super) fn committed_interaction_mutation<T>(
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Interaction {
                    thread_id: batch.cursor_after.thread_id.clone(),
                    interaction_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Interaction,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("interaction commit has one acknowledgement"),
            },
            value,
        }
    }

    pub(super) fn surface_operation_record<'a>(
        snapshot: &'a surface::SurfaceSnapshot,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&'a surface::OperationRecord> {
        snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == *operation_id)
    }

    pub(super) fn surface_tool_for_runtime_request(
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<surface::SurfaceToolRequest> {
        let tool_call_id = surface::SurfaceToolCallId::try_new(request.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == tool_call_id)
            .map(|tool| tool.request.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tool interaction lacks a committed provider tool identity",
                )
            })?;
        let raw_arguments = request.raw_arguments.clone().unwrap_or_default();
        if tool.source_response_id.is_none()
            || tool.turn_id != generation.logical_turn_id
            || tool.name.as_str() != request.name.as_str()
            || tool.action != surface_tool_action(request.action)
            || tool.target.as_ref().map(surface::DisplayText::as_str) != request.target.as_deref()
            || tool.raw_arguments.as_str() != raw_arguments
            || tool.arguments_digest != surface_sha256(raw_arguments.as_bytes())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool interaction differs from the committed provider request",
            ));
        }
        Ok(tool)
    }

    pub(super) fn capability_call_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::capability_call_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
        )
    }

    pub(super) fn capability_call_transition_batch(
        &self,
        calls: Vec<surface::SurfaceCapabilityCall>,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::capability_call_transition_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            calls,
        )
    }

    pub(super) fn ambiguous_capability_tool_events(
        &self,
        call: &surface::SurfaceCapabilityCall,
    ) -> Result<
        Vec<(surface::SurfaceScope, surface::SurfaceEvent)>,
        surface::SurfaceClientCommandError,
    > {
        crate::runtime_actor::capability::ambiguous_capability_tool_events(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
        )
    }

    pub(super) fn ambiguous_write_capability_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
    ) -> Result<surface::SurfaceCommitBatch, surface::SurfaceClientCommandError> {
        crate::runtime_actor::capability::ambiguous_write_capability_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
        )
    }

    pub(super) fn terminal_create_completed_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
        terminal_id: surface::SurfaceRemoteTerminalId,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::terminal_create_completed_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
            terminal_id,
        )
    }

    pub(super) fn ambiguous_terminal_create_capability_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
    ) -> Result<surface::SurfaceCommitBatch, surface::SurfaceClientCommandError> {
        crate::runtime_actor::capability::ambiguous_terminal_create_capability_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
        )
    }

    pub(super) fn terminal_cleanup_started_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
        lease: surface::SurfaceRemoteTerminalLease,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::terminal_cleanup_started_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
            lease,
        )
    }

    pub(super) fn terminal_release_started_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::terminal_release_started_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
        )
    }

    pub(super) fn terminal_cleanup_completed_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
        lease: surface::SurfaceRemoteTerminalLease,
    ) -> surface::SurfaceCommitBatch {
        crate::runtime_actor::capability::terminal_cleanup_completed_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
            lease,
        )
    }

    pub(super) fn ambiguous_terminal_cleanup_capability_batch(
        &self,
        call: surface::SurfaceCapabilityCall,
        lease_id: surface::UuidV7,
        terminal_id: surface::SurfaceRemoteTerminalId,
    ) -> Result<surface::SurfaceCommitBatch, surface::SurfaceClientCommandError> {
        crate::runtime_actor::capability::ambiguous_terminal_cleanup_capability_batch(
            &self.resident_surface.coordinator.state().snapshot(),
            call,
            lease_id,
            terminal_id,
        )
    }
    pub(super) fn retry_surface_capability_transition(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        physical_write_confirmed: bool,
    ) -> bool {
        let Some(RuntimeActorEffect::CommitCapability(effect)) = self
            .resident_surface
            .capability
            .retry_transition_effect(call_id, physical_write_confirmed)
        else {
            return true;
        };
        self.apply_surface_capability_commit(effect)
    }

    pub(super) fn apply_surface_capability_commit(
        &mut self,
        effect: CapabilityCommitEffect,
    ) -> bool {
        let committed = self
            .resident_surface
            .coordinator
            .commit_generation_batch(effect.fence().clone(), effect.batch())
            .is_ok();
        let step = crate::runtime_actor::capability::resolve_capability_commit(
            &mut self.resident_surface.capability,
            effect,
            committed,
        );
        match step {
            CapabilityCommitStep::Retained { reply } => {
                apply_optional_runtime_actor_reply_effect(reply);
                false
            }
            CapabilityCommitStep::Deferred {
                call_id,
                deferred_settlement,
                reply,
            } => {
                apply_optional_runtime_actor_reply_effect(reply);
                self.apply_deferred_surface_capability_settlement(&call_id, deferred_settlement);
                !self.resident_surface.capability.has_transition(&call_id)
            }
            CapabilityCommitStep::Finished { reply } => {
                apply_optional_runtime_actor_reply_effect(reply);
                true
            }
        }
    }

    pub(super) fn apply_deferred_surface_capability_settlement(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        deferred_settlement: PendingSurfaceCapabilitySettlement,
    ) {
        match deferred_settlement {
            PendingSurfaceCapabilitySettlement::ReadTextFile {
                client,
                capability_revision,
                settlement,
            } => {
                let _ = self.settle_surface_acp_read_text_file(
                    &client,
                    call_id.clone(),
                    capability_revision,
                    settlement,
                );
            }
            PendingSurfaceCapabilitySettlement::WriteTextFile {
                client,
                capability_revision,
                settlement,
            } => {
                let _ = self.settle_surface_acp_write_text_file(
                    &client,
                    call_id.clone(),
                    capability_revision,
                    settlement,
                );
            }
            PendingSurfaceCapabilitySettlement::TerminalCreate {
                client,
                capability_revision,
                settlement,
            } => {
                let _ = self.settle_surface_acp_terminal_create(
                    &client,
                    call_id.clone(),
                    capability_revision,
                    settlement,
                );
            }
            PendingSurfaceCapabilitySettlement::TerminalObservation {
                client,
                capability_revision,
                settlement,
            } => {
                let _ = self.settle_surface_acp_terminal_observation(
                    &client,
                    call_id.clone(),
                    capability_revision,
                    settlement,
                );
            }
            PendingSurfaceCapabilitySettlement::TerminalCleanup {
                client,
                capability_revision,
                settlement,
            } => {
                let _ = self.settle_surface_acp_terminal_cleanup(
                    &client,
                    call_id.clone(),
                    capability_revision,
                    settlement,
                );
            }
            PendingSurfaceCapabilitySettlement::DispatchTerminalCleanup { route, dispatch } => {
                #[cfg(test)]
                crate::acp_stall_trace::record("actor_dispatch", &format!("{:?}", call_id));
                self.resident_surface.capability.try_claim_write(call_id);
                if let Err(error) = self
                    .resident_surface
                    .hub
                    .dispatch_acp_terminal_cleanup(&route, dispatch)
                {
                    #[cfg(test)]
                    crate::acp_stall_trace::record("actor_dispatch_err", &format!("{:?}", call_id));
                    let _ = self.settle_surface_terminal_cleanup_ambiguous(
                        call_id,
                        format!(
                            "ACP terminal cleanup dispatch failed after durable retry: {error:?}"
                        ),
                    );
                }
                #[cfg(test)]
                crate::acp_stall_trace::record("actor_dispatch_ok", &format!("{:?}", call_id));
            }
            PendingSurfaceCapabilitySettlement::BeginTerminalRelease {
                kill_call,
                lease_id,
                terminal_id,
            } => {
                if let Some((resident, waiter)) = self
                    .resident_surface
                    .capability
                    .take_call_with_waiter(call_id)
                {
                    let _ = self.begin_surface_terminal_release(
                        kill_call,
                        lease_id,
                        terminal_id,
                        resident,
                        waiter,
                    );
                }
            }
        }
    }

    pub(super) fn settle_surface_capability_transitions_for_shutdown(&mut self) -> bool {
        for _ in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            if self.resident_surface.capability.transitions_empty() {
                return true;
            }
            let mut call_ids = self
                .resident_surface
                .capability
                .pending_transition_ids()
                .cloned()
                .collect::<Vec<_>>();
            call_ids.sort();
            for call_id in call_ids {
                self.retry_surface_capability_transition(&call_id, false);
            }
        }
        self.resident_surface.capability.transitions_empty()
    }

    pub(super) fn request_surface_acp_read_text_file(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_core::tool_types::ToolRequest,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
        reply: SyncSender<io::Result<String>>,
    ) {
        let result = (|| -> io::Result<()> {
            if active.surface_operation.as_ref() != Some(&fence)
                || Self::surface_interaction_admission_closed(active)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime capability generation fence is stale",
                ));
            }
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let surface::OperationOrigin::AcpPrompt { session_id, .. } = &operation.intent.origin
            else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP client read requires an ACP prompt operation",
                ));
            };
            let origin_attachment = self
                .resident_surface
                .interactions
                .operation_origin_attachments
                .get(&fence.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP operation origin attachment is unavailable",
                    )
                })?;
            let route = self
                .resident_surface
                .hub
                .select_acp_capability_attachment(
                    surface::SurfaceCapabilityCallKind::ReadTextFile,
                    origin_attachment,
                )
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP read capability route is unavailable",
                    )
                })?;
            let path = surface::CanonicalPath::try_new(path).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ACP read path: {error:?}"),
                )
            })?;
            let arguments = serde_json::to_vec(&(
                path.as_path()
                    .to_str()
                    .expect("canonical ACP path is valid UTF-8"),
                line,
                limit,
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                fence: fence.clone(),
                capability_revision: route.capability_revision,
                policy_epoch: operation.intent.policy_epoch,
                kind: surface::SurfaceCapabilityCallKind::ReadTextFile,
                arguments_digest: surface_sha256(&arguments),
                owning_tool_call_id: tool.tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            let batch = self.capability_call_batch(call.clone());
            self.commit_surface_generation_batch_with_retry(fence.clone(), &batch)?;
            self.resident_surface.capability.register_call(
                call_id.clone(),
                ResidentSurfaceCapabilityCall::new(
                    route.attachment_id.clone(),
                    route.capability_revision,
                    false,
                    None,
                    Some(ResidentSurfaceCapabilityWaiter::ReadTextFile(reply.clone())),
                ),
            );
            let dispatch = surface::AcpReadTextFileDispatch {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                capability_revision: route.capability_revision,
                path,
                line,
                limit,
            };
            if let Err(error) = self
                .resident_surface
                .hub
                .dispatch_acp_read_text_file(&route, dispatch)
            {
                let diagnostic = surface::SafeDiagnosticText::try_new(format!(
                    "ACP read dispatch failed: {error:?}"
                ))
                .expect("bounded fixed capability diagnostic");
                let mut failed = call;
                failed.state =
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic };
                let failed_batch = self.capability_call_batch(failed);
                let waiter_error = io::Error::new(
                    io::ErrorKind::NotConnected,
                    "ACP read dispatch failed before write",
                );
                if self
                    .commit_surface_generation_batch_with_retry(fence.clone(), &failed_batch)
                    .is_err()
                {
                    self.resident_surface.capability.retain_transition(
                        call_id,
                        fence,
                        failed_batch,
                        Some(ResidentCapabilityController::read_waiter_outcome(Err(
                            waiter_error,
                        ))),
                    );
                    return Ok(());
                }
                self.resident_surface.capability.discard_call(&call_id);
                return Err(waiter_error);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn authorize_surface_capability_settlement(
        &self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: &surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<surface::SurfaceCapabilityCall, surface::SurfaceClientCommandError> {
        if !self.resident_surface.capability.authorize_call(
            call_id,
            client.attachment_id(),
            capability_revision,
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let call = ResidentCapabilityController::surface_call(
            self.resident_surface.coordinator.state().snapshot(),
            call_id,
        )
        .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        if call.capability_revision != capability_revision {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        Ok(call)
    }

    pub(super) fn claim_surface_acp_read_text_file_write(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::ReadTextFile
            || call.state != surface::SurfaceCapabilityCallState::Prepared
            || self.resident_surface.capability.has_transition(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !self.resident_surface.capability.try_claim_write(&call_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Ok(())
    }

    pub(super) fn mark_surface_acp_read_text_file_written(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::ReadTextFile {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            let is_written_transition = self
                .resident_surface
                .capability
                .transition_waits_for_written(&call_id);
            if !is_written_transition {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            return self
                .retry_surface_capability_transition(&call_id, true)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if call.state != surface::SurfaceCapabilityCallState::WrittenAwaitingResponse {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !self
            .resident_surface
            .capability
            .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        self.resident_surface.capability.release_write(&call_id);
        Ok(())
    }

    pub(super) fn settle_surface_acp_read_text_file(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpReadTextFileSettlement,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::ReadTextFile {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            let waits_for_written = self
                .resident_surface
                .capability
                .transition_waits_for_written(&call_id);
            if waits_for_written {
                if self
                    .resident_surface
                    .capability
                    .set_deferred_settlement(
                        &call_id,
                        PendingSurfaceCapabilitySettlement::ReadTextFile {
                            client: client.clone(),
                            capability_revision,
                            settlement,
                        },
                    )
                    .is_err()
                {
                    return Err(surface::SurfaceClientCommandError::Unauthorized);
                }
            }
            return self
                .retry_surface_capability_transition(&call_id, false)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let outcome = crate::runtime_actor::capability::settle_acp_read_text_file_call(
            &mut call, settlement,
        )?;
        let waiter_result = outcome.waiter_result;
        let physically_written_transition = outcome.physically_written;
        let fence = call.fence.clone();
        let mut transitions = physically_written_transition
            .into_iter()
            .collect::<Vec<_>>();
        transitions.push(call);
        let batch = self.capability_call_transition_batch(transitions);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id,
                fence,
                batch,
                Some(ResidentCapabilityController::read_waiter_outcome(
                    waiter_result,
                )),
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        apply_optional_runtime_actor_reply_effect(
            self.resident_surface.capability.apply_committed_transition(
                &call_id,
                Some(ResidentCapabilityController::read_waiter_outcome(
                    waiter_result,
                )),
                false,
            ),
        );
        Ok(())
    }

    pub(super) fn request_surface_acp_write_text_file(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_core::tool_types::ToolRequest,
        path: PathBuf,
        content: String,
        reply: SyncSender<io::Result<()>>,
    ) {
        let result = (|| -> io::Result<()> {
            if active.surface_operation.as_ref() != Some(&fence)
                || Self::surface_interaction_admission_closed(active)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime capability generation fence is stale",
                ));
            }
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let surface::OperationOrigin::AcpPrompt { session_id, .. } = &operation.intent.origin
            else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP client write requires an ACP prompt operation",
                ));
            };
            let origin_attachment = self
                .resident_surface
                .interactions
                .operation_origin_attachments
                .get(&fence.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP operation origin attachment is unavailable",
                    )
                })?;
            let route = self
                .resident_surface
                .hub
                .select_acp_capability_attachment(
                    surface::SurfaceCapabilityCallKind::WriteTextFile,
                    origin_attachment,
                )
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP write capability route is unavailable",
                    )
                })?;
            let path = surface::CanonicalPath::try_new(path).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ACP write path: {error:?}"),
                )
            })?;
            let content_digest = surface_sha256(content.as_bytes());
            let arguments = serde_json::to_vec(&(
                path.as_path()
                    .to_str()
                    .expect("canonical ACP path is valid UTF-8"),
                content_digest,
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                fence: fence.clone(),
                capability_revision: route.capability_revision,
                policy_epoch: operation.intent.policy_epoch,
                kind: surface::SurfaceCapabilityCallKind::WriteTextFile,
                arguments_digest: surface_sha256(&arguments),
                owning_tool_call_id: tool.tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            let batch = self.capability_call_batch(call.clone());
            self.commit_surface_generation_batch_with_retry(fence.clone(), &batch)?;
            self.resident_surface.capability.register_call(
                call_id.clone(),
                ResidentSurfaceCapabilityCall::new(
                    route.attachment_id.clone(),
                    route.capability_revision,
                    false,
                    None,
                    Some(ResidentSurfaceCapabilityWaiter::WriteTextFile(
                        reply.clone(),
                    )),
                ),
            );
            let dispatch = surface::AcpWriteTextFileDispatch {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                capability_revision: route.capability_revision,
                path,
                content,
            };
            if let Err(error) = self
                .resident_surface
                .hub
                .dispatch_acp_write_text_file(&route, dispatch)
            {
                let diagnostic = surface::SafeDiagnosticText::try_new(format!(
                    "ACP write dispatch failed: {error:?}"
                ))
                .expect("bounded fixed capability diagnostic");
                let mut failed = call;
                failed.state =
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic };
                let failed_batch = self.capability_call_batch(failed);
                let waiter_error = io::Error::new(
                    io::ErrorKind::NotConnected,
                    "ACP write dispatch failed before write",
                );
                if self
                    .commit_surface_generation_batch_with_retry(fence.clone(), &failed_batch)
                    .is_err()
                {
                    self.resident_surface.capability.retain_transition(
                        call_id,
                        fence,
                        failed_batch,
                        Some(ResidentCapabilityController::write_waiter_outcome(Err(
                            waiter_error,
                        ))),
                    );
                    return Ok(());
                }
                self.resident_surface.capability.discard_call(&call_id);
                return Err(waiter_error);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn permit_surface_acp_write_text_file_delivery(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::WriteTextFile
            || call.state != surface::SurfaceCapabilityCallState::Prepared
            || self.resident_surface.capability.has_transition(&call_id)
            || self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::DeliveryPossible;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !self.resident_surface.capability.try_claim_write(&call_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        Ok(())
    }

    pub(super) fn mark_surface_acp_write_text_file_written(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::WriteTextFile {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            return self
                .retry_surface_capability_transition(&call_id, true)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if call.state != surface::SurfaceCapabilityCallState::DeliveryPossible
            || !self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.resident_surface.capability.release_write(&call_id);
        Ok(())
    }

    pub(super) fn settle_surface_acp_write_text_file(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpWriteTextFileSettlement,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::WriteTextFile {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            if self
                .resident_surface
                .capability
                .set_deferred_settlement(
                    &call_id,
                    PendingSurfaceCapabilitySettlement::WriteTextFile {
                        client: client.clone(),
                        capability_revision,
                        settlement,
                    },
                )
                .is_err()
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            return self
                .retry_surface_capability_transition(&call_id, false)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let waiter_result = crate::runtime_actor::capability::settle_acp_write_text_file_call(
            &mut call, settlement,
        )?;
        let fence = call.fence.clone();
        let batch = if matches!(
            &call.state,
            surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                effect_kind: surface::ExternalEffectKind::FileWrite,
                ..
            }
        ) {
            self.ambiguous_write_capability_batch(call)?
        } else {
            self.capability_call_batch(call)
        };
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id,
                fence,
                batch,
                Some(ResidentCapabilityController::write_waiter_outcome(
                    waiter_result,
                )),
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        apply_optional_runtime_actor_reply_effect(
            self.resident_surface.capability.apply_committed_transition(
                &call_id,
                Some(ResidentCapabilityController::write_waiter_outcome(
                    waiter_result,
                )),
                false,
            ),
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_surface_acp_terminal_create(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_core::tool_types::ToolRequest,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<u64>,
        reply: SyncSender<io::Result<String>>,
    ) {
        let result = (|| -> io::Result<()> {
            if active.surface_operation.as_ref() != Some(&fence)
                || Self::surface_interaction_admission_closed(active)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime capability generation fence is stale",
                ));
            }
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let surface::OperationOrigin::AcpPrompt { session_id, .. } = &operation.intent.origin
            else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP terminal create requires an ACP prompt operation",
                ));
            };
            let origin_attachment = self
                .resident_surface
                .interactions
                .operation_origin_attachments
                .get(&fence.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP operation origin attachment is unavailable",
                    )
                })?;
            let route = self
                .resident_surface
                .hub
                .select_acp_capability_attachment(
                    surface::SurfaceCapabilityCallKind::TerminalCreate,
                    origin_attachment,
                )
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP terminal capability route is unavailable",
                    )
                })?;
            if command.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ACP terminal command is empty",
                ));
            }
            let cwd = cwd
                .map(surface::CanonicalPath::try_new)
                .transpose()
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid ACP terminal cwd: {error:?}"),
                    )
                })?;
            let arguments = serde_json::to_vec(&(
                &command,
                &args,
                &env,
                cwd.as_ref().map(|path| path.as_path()),
                output_byte_limit,
            ))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                fence: fence.clone(),
                capability_revision: route.capability_revision,
                policy_epoch: operation.intent.policy_epoch,
                kind: surface::SurfaceCapabilityCallKind::TerminalCreate,
                arguments_digest: surface_sha256(&arguments),
                owning_tool_call_id: tool.tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            let batch = self.capability_call_batch(call.clone());
            self.commit_surface_generation_batch_with_retry(fence.clone(), &batch)?;
            self.resident_surface.capability.register_call(
                call_id.clone(),
                ResidentSurfaceCapabilityCall::new(
                    route.attachment_id.clone(),
                    route.capability_revision,
                    false,
                    None,
                    Some(ResidentSurfaceCapabilityWaiter::TerminalCreate(
                        reply.clone(),
                    )),
                ),
            );
            let dispatch = surface::AcpTerminalCreateDispatch {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                capability_revision: route.capability_revision,
                command,
                args,
                env,
                cwd,
                output_byte_limit,
            };
            if let Err(error) = self
                .resident_surface
                .hub
                .dispatch_acp_terminal_create(&route, dispatch)
            {
                let diagnostic = surface::SafeDiagnosticText::try_new(format!(
                    "ACP terminal create dispatch failed: {error:?}"
                ))
                .expect("bounded fixed capability diagnostic");
                let mut failed = call;
                failed.state =
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic };
                let failed_batch = self.capability_call_batch(failed);
                let waiter_error = io::Error::new(
                    io::ErrorKind::NotConnected,
                    "ACP terminal create dispatch failed before write",
                );
                if self
                    .commit_surface_generation_batch_with_retry(fence.clone(), &failed_batch)
                    .is_err()
                {
                    self.resident_surface.capability.retain_transition(
                        call_id,
                        fence,
                        failed_batch,
                        Some(
                            ResidentCapabilityController::terminal_create_waiter_outcome(Err(
                                waiter_error,
                            )),
                        ),
                    );
                    return Ok(());
                }
                self.resident_surface.capability.discard_call(&call_id);
                return Err(waiter_error);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn permit_surface_acp_terminal_create_delivery(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::TerminalCreate
            || call.state != surface::SurfaceCapabilityCallState::Prepared
            || self.resident_surface.capability.has_transition(&call_id)
            || self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::DeliveryPossible;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !self.resident_surface.capability.try_claim_write(&call_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        Ok(())
    }

    pub(super) fn mark_surface_acp_terminal_create_written(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::TerminalCreate {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            return self
                .retry_surface_capability_transition(&call_id, true)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if call.state != surface::SurfaceCapabilityCallState::DeliveryPossible
            || !self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.resident_surface.capability.release_write(&call_id);
        Ok(())
    }

    pub(super) fn settle_surface_acp_terminal_create(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalCreateSettlement,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if call.kind != surface::SurfaceCapabilityCallKind::TerminalCreate {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            if self
                .resident_surface
                .capability
                .set_deferred_settlement(
                    &call_id,
                    PendingSurfaceCapabilitySettlement::TerminalCreate {
                        client: client.clone(),
                        capability_revision,
                        settlement,
                    },
                )
                .is_err()
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            return self
                .retry_surface_capability_transition(&call_id, false)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let outcome = crate::runtime_actor::capability::settle_acp_terminal_create_call(
            &mut call, settlement,
        )?;
        let waiter_result = outcome.waiter_result;
        let completed_terminal_id = outcome.completed_terminal_id;
        let fence = call.fence.clone();
        let batch = if let Some(terminal_id) = completed_terminal_id {
            self.terminal_create_completed_batch(call, terminal_id)
        } else if matches!(
            &call.state,
            surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                effect_kind: surface::ExternalEffectKind::TerminalCreate,
                ..
            }
        ) {
            self.ambiguous_terminal_create_capability_batch(call)?
        } else {
            self.capability_call_batch(call)
        };
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id,
                fence,
                batch,
                Some(ResidentCapabilityController::terminal_create_waiter_outcome(waiter_result)),
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        apply_optional_runtime_actor_reply_effect(
            self.resident_surface.capability.apply_committed_transition(
                &call_id,
                Some(ResidentCapabilityController::terminal_create_waiter_outcome(waiter_result)),
                false,
            ),
        );
        Ok(())
    }

    pub(super) fn request_surface_acp_terminal_observation(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_core::tool_types::ToolRequest,
        terminal_id: String,
        kind: surface::SurfaceCapabilityCallKind,
        reply: SyncSender<io::Result<RuntimeAcpTerminalObservation>>,
    ) {
        let result = (|| -> io::Result<()> {
            if active.surface_operation.as_ref() != Some(&fence)
                || Self::surface_interaction_admission_closed(active)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime capability generation fence is stale",
                ));
            }
            if !matches!(
                kind,
                surface::SurfaceCapabilityCallKind::TerminalOutput
                    | surface::SurfaceCapabilityCallKind::TerminalWaitForExit
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid ACP terminal observation kind",
                ));
            }
            let terminal_id =
                surface::SurfaceRemoteTerminalId::try_new(terminal_id).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid ACP terminal identity")
                })?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let has_live_lease = snapshot
                .tools
                .iter()
                .find(|candidate| candidate.request.tool_call_id == tool.tool_call_id)
                .is_some_and(|tool| {
                    tool.terminal_leases.iter().any(|lease| {
                        matches!(
                            &lease.state,
                            surface::SurfaceRemoteTerminalLeaseState::Live {
                                terminal_id: lease_terminal_id,
                                owner_fence,
                            } if lease_terminal_id == &terminal_id && owner_fence == &fence
                        )
                    })
                });
            if !has_live_lease {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ACP terminal observation requires the exact live runtime lease",
                ));
            }
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let surface::OperationOrigin::AcpPrompt { session_id, .. } = &operation.intent.origin
            else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP terminal observation requires an ACP prompt operation",
                ));
            };
            let origin_attachment = self
                .resident_surface
                .interactions
                .operation_origin_attachments
                .get(&fence.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP operation origin attachment is unavailable",
                    )
                })?;
            let route = self
                .resident_surface
                .hub
                .select_acp_capability_attachment(kind, origin_attachment)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP terminal observation capability route is unavailable",
                    )
                })?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                fence: fence.clone(),
                capability_revision: route.capability_revision,
                policy_epoch: operation.intent.policy_epoch,
                kind,
                arguments_digest: surface_sha256(terminal_id.as_str().as_bytes()),
                owning_tool_call_id: tool.tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            let batch = self.capability_call_batch(call.clone());
            self.commit_surface_generation_batch_with_retry(fence.clone(), &batch)?;
            self.resident_surface.capability.register_call(
                call_id.clone(),
                ResidentSurfaceCapabilityCall::new(
                    route.attachment_id.clone(),
                    route.capability_revision,
                    false,
                    None,
                    Some(ResidentSurfaceCapabilityWaiter::TerminalObservation(
                        reply.clone(),
                    )),
                ),
            );
            let dispatch = surface::AcpTerminalObservationDispatch {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                capability_revision: route.capability_revision,
                terminal_id,
                kind,
            };
            if let Err(error) = self
                .resident_surface
                .hub
                .dispatch_acp_terminal_observation(&route, dispatch)
            {
                let diagnostic = surface::SafeDiagnosticText::try_new(format!(
                    "ACP terminal observation dispatch failed: {error:?}"
                ))
                .expect("bounded fixed capability diagnostic");
                let mut failed = call;
                failed.state =
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic };
                let failed_batch = self.capability_call_batch(failed);
                let waiter_error = io::Error::new(
                    io::ErrorKind::NotConnected,
                    "ACP terminal observation dispatch failed before write",
                );
                if self
                    .commit_surface_generation_batch_with_retry(fence.clone(), &failed_batch)
                    .is_err()
                {
                    self.resident_surface.capability.retain_transition(
                        call_id,
                        fence,
                        failed_batch,
                        Some(
                            ResidentCapabilityController::terminal_observation_waiter_outcome(Err(
                                waiter_error,
                            )),
                        ),
                    );
                    return Ok(());
                }
                self.resident_surface.capability.discard_call(&call_id);
                return Err(waiter_error);
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn claim_surface_acp_terminal_observation_write(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalOutput
                | surface::SurfaceCapabilityCallKind::TerminalWaitForExit
        ) || call.state != surface::SurfaceCapabilityCallState::Prepared
            || self.resident_surface.capability.has_transition(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !self.resident_surface.capability.try_claim_write(&call_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Ok(())
    }

    pub(super) fn mark_surface_acp_terminal_observation_written(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalOutput
                | surface::SurfaceCapabilityCallKind::TerminalWaitForExit
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            return self
                .retry_surface_capability_transition(&call_id, true)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if call.state != surface::SurfaceCapabilityCallState::WrittenAwaitingResponse
            || !self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        self.resident_surface.capability.release_write(&call_id);
        Ok(())
    }

    pub(super) fn settle_surface_acp_terminal_observation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalObservationSettlement,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalOutput
                | surface::SurfaceCapabilityCallKind::TerminalWaitForExit
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            if self
                .resident_surface
                .capability
                .set_deferred_settlement(
                    &call_id,
                    PendingSurfaceCapabilitySettlement::TerminalObservation {
                        client: client.clone(),
                        capability_revision,
                        settlement,
                    },
                )
                .is_err()
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            return self
                .retry_surface_capability_transition(&call_id, false)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let accepted_state = match &settlement {
            surface::AcpTerminalObservationSettlement::FailedBeforeWrite { .. } => {
                call.state == surface::SurfaceCapabilityCallState::Prepared
            }
            _ => matches!(
                call.state,
                surface::SurfaceCapabilityCallState::Prepared
                    | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse
            ),
        };
        if !accepted_state {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let physically_written_transition = (call.state
            == surface::SurfaceCapabilityCallState::Prepared
            && !matches!(
                &settlement,
                surface::AcpTerminalObservationSettlement::FailedBeforeWrite { .. }
            ))
        .then(|| {
            let mut written = call.clone();
            written.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
            written
        });
        let waiter_result = match settlement {
            surface::AcpTerminalObservationSettlement::Output {
                output,
                truncated,
                exit_status,
            } if call.kind == surface::SurfaceCapabilityCallKind::TerminalOutput => {
                match surface::AcpCapabilityText::try_new(output) {
                    Ok(output) => {
                        let result = surface::CapabilityCallResult::TerminalOutputObserved {
                            output: output.clone(),
                            truncated,
                            exit_status: exit_status.clone(),
                        };
                        match serde_json::to_vec(&result) {
                            Ok(canonical)
                                if canonical.len() as u64
                                    <= surface::ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT =>
                            {
                                call.state = surface::SurfaceCapabilityCallState::Completed {
                                    response_digest: surface_sha256(&canonical),
                                    result,
                                };
                                Ok(RuntimeAcpTerminalObservation::Output(
                                    RuntimeAcpTerminalOutput {
                                        output: output.as_str().to_string(),
                                        truncated,
                                        exit_status: exit_status.map(runtime_terminal_exit_status),
                                    },
                                ))
                            }
                            _ => {
                                let message =
                                    "ACP terminal output exceeded the durable result limit";
                                call.state =
                                    surface::SurfaceCapabilityCallState::ObservationUnavailable {
                                        error: surface::SafeDiagnosticText::try_new(message)
                                            .expect("fixed capability diagnostic is bounded"),
                                    };
                                Err(io::Error::new(io::ErrorKind::InvalidData, message))
                            }
                        }
                    }
                    Err(_) => {
                        let message = "ACP terminal output was invalid or too large";
                        call.state = surface::SurfaceCapabilityCallState::ObservationUnavailable {
                            error: surface::SafeDiagnosticText::try_new(message)
                                .expect("fixed capability diagnostic is bounded"),
                        };
                        Err(io::Error::new(io::ErrorKind::InvalidData, message))
                    }
                }
            }
            surface::AcpTerminalObservationSettlement::Exit { exit_status }
                if call.kind == surface::SurfaceCapabilityCallKind::TerminalWaitForExit =>
            {
                let result = surface::CapabilityCallResult::TerminalExitObserved {
                    exit_status: exit_status.clone(),
                };
                match serde_json::to_vec(&result) {
                    Ok(canonical)
                        if canonical.len() as u64
                            <= surface::ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT =>
                    {
                        call.state = surface::SurfaceCapabilityCallState::Completed {
                            response_digest: surface_sha256(&canonical),
                            result,
                        };
                        Ok(RuntimeAcpTerminalObservation::Exit(
                            runtime_terminal_exit_status(exit_status),
                        ))
                    }
                    _ => {
                        let message =
                            "ACP terminal wait response exceeded the durable result limit";
                        call.state = surface::SurfaceCapabilityCallState::ObservationUnavailable {
                            error: surface::SafeDiagnosticText::try_new(message)
                                .expect("fixed capability diagnostic is bounded"),
                        };
                        Err(io::Error::new(io::ErrorKind::InvalidData, message))
                    }
                }
            }
            surface::AcpTerminalObservationSettlement::RemoteError { code, message } => {
                let code = surface::AcpCapabilityIdentifier::try_new(code).unwrap_or_else(|_| {
                    surface::AcpCapabilityIdentifier::try_new("unknown")
                        .expect("fixed capability error code is bounded")
                });
                let message = surface::SafeDiagnosticText::try_new(message).unwrap_or_else(|_| {
                    surface::SafeDiagnosticText::try_new(
                        "ACP terminal observation returned an invalid remote diagnostic",
                    )
                    .expect("fixed capability diagnostic is bounded")
                });
                let waiter_error = format!("ACP terminal observation failed: {}", message.as_str());
                let result = surface::CapabilityCallResult::RemoteError {
                    code,
                    message: message.clone(),
                };
                let canonical = serde_json::to_vec(&result)
                    .expect("bounded capability error result is serializable");
                call.state = surface::SurfaceCapabilityCallState::Completed {
                    response_digest: surface_sha256(&canonical),
                    result,
                };
                Err(io::Error::other(waiter_error))
            }
            surface::AcpTerminalObservationSettlement::FailedBeforeWrite { message } => {
                let diagnostic =
                    surface::SafeDiagnosticText::try_new(message).unwrap_or_else(|_| {
                        surface::SafeDiagnosticText::try_new(
                            "ACP terminal observation failed before write",
                        )
                        .expect("fixed capability diagnostic is bounded")
                    });
                let waiter_error = diagnostic.as_str().to_string();
                call.state =
                    surface::SurfaceCapabilityCallState::FailedBeforeWrite { error: diagnostic };
                Err(io::Error::new(io::ErrorKind::NotConnected, waiter_error))
            }
            surface::AcpTerminalObservationSettlement::ObservationUnavailable { message } => {
                let diagnostic =
                    surface::SafeDiagnosticText::try_new(message).unwrap_or_else(|_| {
                        surface::SafeDiagnosticText::try_new(
                            "ACP terminal observation response was unavailable",
                        )
                        .expect("fixed capability diagnostic is bounded")
                    });
                let waiter_error = diagnostic.as_str().to_string();
                call.state = surface::SurfaceCapabilityCallState::ObservationUnavailable {
                    error: diagnostic,
                };
                Err(io::Error::new(io::ErrorKind::NotConnected, waiter_error))
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let fence = call.fence.clone();
        let mut transitions = physically_written_transition
            .into_iter()
            .collect::<Vec<_>>();
        transitions.push(call);
        let batch = self.capability_call_transition_batch(transitions);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id,
                fence,
                batch,
                Some(
                    ResidentCapabilityController::terminal_observation_waiter_outcome(
                        waiter_result,
                    ),
                ),
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        apply_optional_runtime_actor_reply_effect(
            self.resident_surface.capability.apply_committed_transition(
                &call_id,
                Some(
                    ResidentCapabilityController::terminal_observation_waiter_outcome(
                        waiter_result,
                    ),
                ),
                false,
            ),
        );
        Ok(())
    }

    pub(super) fn request_surface_acp_terminal_cleanup(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_core::tool_types::ToolRequest,
        terminal_id: String,
        reply: SyncSender<io::Result<()>>,
    ) {
        let result = (|| -> io::Result<()> {
            if active.surface_operation.as_ref() != Some(&fence)
                || Self::surface_interaction_admission_closed(active)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime terminal cleanup generation fence is stale",
                ));
            }
            let terminal_id =
                surface::SurfaceRemoteTerminalId::try_new(terminal_id).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "runtime terminal cleanup identity is invalid",
                    )
                })?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let surface::OperationOrigin::AcpPrompt { session_id, .. } = &operation.intent.origin
            else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP terminal cleanup requires an ACP prompt operation",
                ));
            };
            let lease = snapshot
                .tools
                .iter()
                .find(|candidate| candidate.request.tool_call_id == tool.tool_call_id)
                .and_then(|candidate| {
                    candidate.terminal_leases.iter().find(|lease| {
                        matches!(
                            &lease.state,
                            surface::SurfaceRemoteTerminalLeaseState::Live {
                                terminal_id: live_terminal_id,
                                owner_fence,
                            } if live_terminal_id == &terminal_id && owner_fence == &fence
                        )
                    })
                })
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "runtime-owned live terminal lease is unavailable",
                    )
                })?;
            let origin_attachment = self
                .resident_surface
                .interactions
                .operation_origin_attachments
                .get(&fence.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP operation origin attachment is unavailable",
                    )
                })?;
            let route = self
                .resident_surface
                .hub
                .select_acp_capability_attachment(
                    surface::SurfaceCapabilityCallKind::TerminalKill,
                    origin_attachment,
                )
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ACP terminal cleanup route is unavailable",
                    )
                })?;
            let call_id =
                surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let call = surface::SurfaceCapabilityCall {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                fence: fence.clone(),
                capability_revision: route.capability_revision,
                policy_epoch: operation.intent.policy_epoch,
                kind: surface::SurfaceCapabilityCallKind::TerminalKill,
                arguments_digest: surface_sha256(terminal_id.as_str().as_bytes()),
                owning_tool_call_id: tool.tool_call_id,
                state: surface::SurfaceCapabilityCallState::Prepared,
            };
            let mut kill_pending_lease = lease;
            kill_pending_lease.state = surface::SurfaceRemoteTerminalLeaseState::KillPending {
                terminal_id: terminal_id.clone(),
                owner_fence: fence.clone(),
            };
            let cleanup_lease_id = kill_pending_lease.lease_id.clone();
            let batch = self.terminal_cleanup_started_batch(call.clone(), kill_pending_lease);
            self.resident_surface.capability.register_call(
                call_id.clone(),
                ResidentSurfaceCapabilityCall::new(
                    route.attachment_id.clone(),
                    route.capability_revision,
                    true,
                    Some(ResidentTerminalCleanupLease {
                        lease_id: cleanup_lease_id,
                        terminal_id: terminal_id.clone(),
                    }),
                    Some(ResidentSurfaceCapabilityWaiter::TerminalCleanup(
                        reply.clone(),
                    )),
                ),
            );
            let dispatch = surface::AcpTerminalCleanupDispatch {
                call_id: call_id.clone(),
                acp_session_id: session_id.clone(),
                capability_revision: route.capability_revision,
                terminal_id,
                kind: surface::SurfaceCapabilityCallKind::TerminalKill,
            };
            if self
                .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
                .is_err()
            {
                self.resident_surface.capability.retain_transition(
                    call_id.clone(),
                    fence,
                    batch,
                    None,
                );
                assert!(
                    self.resident_surface
                        .capability
                        .set_deferred_settlement(
                            &call_id,
                            PendingSurfaceCapabilitySettlement::DispatchTerminalCleanup {
                                route,
                                dispatch,
                            },
                        )
                        .is_ok(),
                    "retained terminal cleanup admission"
                );
                return Ok(());
            }
            #[cfg(test)]
            crate::acp_stall_trace::record("actor_dispatch", &format!("{:?}", call_id));
            if let Err(error) = self
                .resident_surface
                .hub
                .dispatch_acp_terminal_cleanup(&route, dispatch)
            {
                self.settle_surface_terminal_cleanup_ambiguous(
                    &call_id,
                    format!("ACP terminal kill dispatch failed after durable admission: {error:?}"),
                )
                .map_err(|error| {
                    io::Error::other(format!(
                        "failed to persist terminal cleanup ambiguity: {error:?}"
                    ))
                })?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    pub(super) fn mark_surface_acp_terminal_cleanup_written(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalKill
                | surface::SurfaceCapabilityCallKind::TerminalRelease
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            return self
                .retry_surface_capability_transition(&call_id, true)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if call.state != surface::SurfaceCapabilityCallState::DeliveryPossible
            || !self
                .resident_surface
                .capability
                .call_write_claimed(&call_id)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        call.state = surface::SurfaceCapabilityCallState::WrittenAwaitingResponse;
        let fence = call.fence.clone();
        let batch = self.capability_call_batch(call);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface
                .capability
                .retain_transition(call_id, fence, batch, None);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.resident_surface.capability.release_write(&call_id);
        Ok(())
    }

    pub(super) fn settle_surface_acp_terminal_cleanup(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        call_id: surface::SurfaceCapabilityCallId,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalCleanupSettlement,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        #[cfg(test)]
        crate::acp_stall_trace::record("actor_settle_start", &format!("{:?}", call_id));
        let call =
            self.authorize_surface_capability_settlement(client, &call_id, capability_revision)?;
        if !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalKill
                | surface::SurfaceCapabilityCallKind::TerminalRelease
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.resident_surface.capability.has_transition(&call_id) {
            if self
                .resident_surface
                .capability
                .set_deferred_settlement(
                    &call_id,
                    PendingSurfaceCapabilitySettlement::TerminalCleanup {
                        client: client.clone(),
                        capability_revision,
                        settlement,
                    },
                )
                .is_err()
            {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            return self
                .retry_surface_capability_transition(&call_id, false)
                .then_some(())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        #[cfg(test)]
        let settle_detail = format!("{:?}", call_id);
        let result = match settlement {
            surface::AcpTerminalCleanupSettlement::Completed
                if call.state == surface::SurfaceCapabilityCallState::WrittenAwaitingResponse =>
            {
                self.complete_surface_terminal_cleanup(call_id, call)
            }
            surface::AcpTerminalCleanupSettlement::RemoteError { code, message }
                if call.state == surface::SurfaceCapabilityCallState::WrittenAwaitingResponse =>
            {
                self.settle_surface_terminal_cleanup_ambiguous(
                    &call_id,
                    format!("ACP terminal cleanup failed remotely ({code}): {message}"),
                )
            }
            surface::AcpTerminalCleanupSettlement::ExternalEffectAmbiguous { message }
                if matches!(
                    call.state,
                    surface::SurfaceCapabilityCallState::DeliveryPossible
                        | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse
                ) =>
            {
                self.settle_surface_terminal_cleanup_ambiguous(&call_id, message)
            }
            _ => Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        #[cfg(test)]
        crate::acp_stall_trace::record("actor_settle_finish", &settle_detail);
        result
    }

    pub(super) fn complete_surface_terminal_cleanup(
        &mut self,
        call_id: surface::SurfaceCapabilityCallId,
        mut call: surface::SurfaceCapabilityCall,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let cleanup_lease = self
            .resident_surface
            .capability
            .terminal_cleanup_lease(&call_id)
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let lease = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
            .and_then(|tool| {
                tool.terminal_leases.iter().find(|lease| {
                    lease.lease_id == cleanup_lease.lease_id
                        && matches!(
                                (&call.kind, &lease.state),
                            (
                                surface::SurfaceCapabilityCallKind::TerminalKill,
                                surface::SurfaceRemoteTerminalLeaseState::KillPending {
                                    terminal_id,
                                    owner_fence,
                                }
                            ) | (
                                surface::SurfaceCapabilityCallKind::TerminalRelease,
                                surface::SurfaceRemoteTerminalLeaseState::ReleasePending {
                                    terminal_id,
                                    owner_fence,
                                }
                            ) if owner_fence == &call.fence
                                && terminal_id == &cleanup_lease.terminal_id
                        )
                })
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let terminal_id = match &lease.state {
            surface::SurfaceRemoteTerminalLeaseState::KillPending { terminal_id, .. }
            | surface::SurfaceRemoteTerminalLeaseState::ReleasePending { terminal_id, .. } => {
                terminal_id.clone()
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let result = match call.kind {
            surface::SurfaceCapabilityCallKind::TerminalKill => {
                surface::CapabilityCallResult::TerminalKillAcknowledged
            }
            surface::SurfaceCapabilityCallKind::TerminalRelease => {
                surface::CapabilityCallResult::TerminalReleaseAcknowledged
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let canonical =
            serde_json::to_vec(&result).expect("terminal cleanup result is serializable");
        call.state = surface::SurfaceCapabilityCallState::Completed {
            response_digest: surface_sha256(&canonical),
            result,
        };
        let mut next_lease = lease.clone();
        next_lease.state = match call.kind {
            surface::SurfaceCapabilityCallKind::TerminalKill => {
                surface::SurfaceRemoteTerminalLeaseState::ReleasePending {
                    terminal_id: terminal_id.clone(),
                    owner_fence: call.fence.clone(),
                }
            }
            surface::SurfaceCapabilityCallKind::TerminalRelease => {
                surface::SurfaceRemoteTerminalLeaseState::Released
            }
            _ => unreachable!("guarded terminal cleanup kind"),
        };
        let fence = call.fence.clone();
        let kind = call.kind;
        let batch = self.terminal_cleanup_completed_batch(call.clone(), next_lease);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            let waiter_outcome = (kind == surface::SurfaceCapabilityCallKind::TerminalRelease)
                .then_some(PendingSurfaceCapabilityWaiterOutcome::TerminalCleanupCompleted);
            self.resident_surface.capability.retain_transition(
                call_id.clone(),
                fence,
                batch,
                waiter_outcome,
            );
            if kind == surface::SurfaceCapabilityCallKind::TerminalKill {
                assert!(
                    self.resident_surface
                        .capability
                        .set_deferred_settlement(
                            &call_id,
                            PendingSurfaceCapabilitySettlement::BeginTerminalRelease {
                                kill_call: call,
                                lease_id: lease.lease_id,
                                terminal_id,
                            },
                        )
                        .is_ok(),
                    "retained terminal kill settlement"
                );
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if kind == surface::SurfaceCapabilityCallKind::TerminalRelease {
            apply_optional_runtime_actor_reply_effect(
                self.resident_surface.capability.apply_committed_transition(
                    &call_id,
                    Some(PendingSurfaceCapabilityWaiterOutcome::TerminalCleanupCompleted),
                    false,
                ),
            );
            return Ok(());
        }
        let (resident, waiter) = self
            .resident_surface
            .capability
            .take_call_with_waiter(&call_id)
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        self.begin_surface_terminal_release(call, lease.lease_id, terminal_id, resident, waiter)
    }

    pub(super) fn begin_surface_terminal_release(
        &mut self,
        kill_call: surface::SurfaceCapabilityCall,
        lease_id: surface::UuidV7,
        terminal_id: surface::SurfaceRemoteTerminalId,
        resident: ResidentSurfaceCapabilityCall,
        waiter: ResidentSurfaceCapabilityWaiter,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let call_id =
            surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let capability_revision = resident.capability_revision();
        let release_call = surface::SurfaceCapabilityCall {
            call_id: call_id.clone(),
            acp_session_id: kill_call.acp_session_id.clone(),
            fence: kill_call.fence.clone(),
            capability_revision,
            policy_epoch: kill_call.policy_epoch,
            kind: surface::SurfaceCapabilityCallKind::TerminalRelease,
            arguments_digest: surface_sha256(terminal_id.as_str().as_bytes()),
            owning_tool_call_id: kill_call.owning_tool_call_id,
            state: surface::SurfaceCapabilityCallState::Prepared,
        };
        let batch = self.terminal_release_started_batch(release_call.clone());
        let attachment_id = resident.attachment_id().clone();
        let route = surface::AcpCapabilityAttachmentRoute {
            attachment_id: attachment_id.clone(),
            capability_revision,
        };
        self.resident_surface.capability.register_call(
            call_id.clone(),
            ResidentSurfaceCapabilityCall::new(
                attachment_id,
                capability_revision,
                true,
                Some(ResidentTerminalCleanupLease {
                    lease_id,
                    terminal_id: terminal_id.clone(),
                }),
                Some(waiter),
            ),
        );
        let dispatch = surface::AcpTerminalCleanupDispatch {
            call_id: call_id.clone(),
            acp_session_id: release_call.acp_session_id,
            capability_revision,
            terminal_id,
            kind: surface::SurfaceCapabilityCallKind::TerminalRelease,
        };
        if self
            .commit_surface_generation_batch_with_retry(release_call.fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id.clone(),
                release_call.fence,
                batch,
                None,
            );
            assert!(
                self.resident_surface
                    .capability
                    .set_deferred_settlement(
                        &call_id,
                        PendingSurfaceCapabilitySettlement::DispatchTerminalCleanup {
                            route,
                            dispatch
                        },
                    )
                    .is_ok(),
                "retained terminal release admission"
            );
            return Ok(());
        }
        #[cfg(test)]
        crate::acp_stall_trace::record("actor_dispatch", &format!("{:?}", call_id));
        if let Err(error) = self
            .resident_surface
            .hub
            .dispatch_acp_terminal_cleanup(&route, dispatch)
        {
            self.settle_surface_terminal_cleanup_ambiguous(
                &call_id,
                format!("ACP terminal release dispatch failed after durable admission: {error:?}"),
            )?;
        }
        Ok(())
    }

    pub(super) fn settle_surface_terminal_cleanup_ambiguous(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        message: String,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut call = ResidentCapabilityController::surface_call(
            self.resident_surface.coordinator.state().snapshot(),
            call_id,
        )
        .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        if !matches!(
            call.state,
            surface::SurfaceCapabilityCallState::DeliveryPossible
                | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse
        ) || !matches!(
            call.kind,
            surface::SurfaceCapabilityCallKind::TerminalKill
                | surface::SurfaceCapabilityCallKind::TerminalRelease
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let cleanup_lease = self
            .resident_surface
            .capability
            .terminal_cleanup_lease(call_id)
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let lease = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
            .and_then(|tool| {
                tool.terminal_leases.iter().find(|lease| {
                    lease.lease_id == cleanup_lease.lease_id
                        && matches!(
                            &lease.state,
                            surface::SurfaceRemoteTerminalLeaseState::KillPending {
                                terminal_id,
                                owner_fence,
                            } | surface::SurfaceRemoteTerminalLeaseState::ReleasePending {
                                terminal_id,
                                owner_fence,
                            } if owner_fence == &call.fence
                                && terminal_id == &cleanup_lease.terminal_id
                        )
                })
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let terminal_id = match lease.state {
            surface::SurfaceRemoteTerminalLeaseState::KillPending { terminal_id, .. }
            | surface::SurfaceRemoteTerminalLeaseState::ReleasePending { terminal_id, .. } => {
                terminal_id
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let effect_kind = match call.kind {
            surface::SurfaceCapabilityCallKind::TerminalKill => {
                surface::ExternalEffectKind::TerminalKill
            }
            surface::SurfaceCapabilityCallKind::TerminalRelease => {
                surface::ExternalEffectKind::TerminalRelease
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let diagnostic = surface::SafeDiagnosticText::try_new(message).unwrap_or_else(|_| {
            surface::SafeDiagnosticText::try_new("ACP terminal cleanup effect is ambiguous")
                .expect("fixed terminal cleanup diagnostic is bounded")
        });
        let waiter_error = diagnostic.as_str().to_string();
        call.state = surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
            effect_kind,
            error: diagnostic,
        };
        let fence = call.fence.clone();
        let batch =
            self.ambiguous_terminal_cleanup_capability_batch(call, lease.lease_id, terminal_id)?;
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &batch)
            .is_err()
        {
            self.resident_surface.capability.retain_transition(
                call_id.clone(),
                fence,
                batch,
                Some(PendingSurfaceCapabilityWaiterOutcome::Failed {
                    kind: io::ErrorKind::Other,
                    message: waiter_error,
                }),
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        apply_optional_runtime_actor_reply_effect(
            self.resident_surface.capability.apply_committed_transition(
                call_id,
                Some(PendingSurfaceCapabilityWaiterOutcome::Failed {
                    kind: io::ErrorKind::Other,
                    message: waiter_error,
                }),
                false,
            ),
        );
        Ok(())
    }
}

#[cfg(test)]
mod task_transcript_query_tests {
    use super::*;
    use orca_core::budget::BudgetUsage;

    fn surface_task(
        task_id: &str,
        parent_task_id: Option<&str>,
        revision: u64,
    ) -> surface::SurfaceTask {
        surface::SurfaceTask {
            task_id: surface::SurfaceTaskId::try_new(task_id).expect("task id"),
            revision: surface::TaskRevision::try_new(revision).expect("task revision"),
            task_type: surface::SurfaceTaskType::Subagent,
            status: surface::SurfaceTaskStatus::Running,
            backgrounded: false,
            description: surface::DisplayText::new("child"),
            created_at: surface::UnixMillis::new(1),
            started_at: Some(surface::UnixMillis::new(1)),
            completed_at: None,
            parent_operation: None,
            parent_task_id: parent_task_id
                .map(|parent| surface::SurfaceTaskId::try_new(parent).expect("parent id")),
            background_fence: None,
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        }
    }

    fn transcript_record(
        task_id: &str,
        parent_task_id: Option<&str>,
        registry_publication_revision: u64,
    ) -> crate::tasks::TaskTranscriptRecord {
        crate::tasks::TaskTranscriptRecord {
            task_id: task_id.to_string(),
            parent_task_id: parent_task_id.map(str::to_string),
            publication_revision: registry_publication_revision,
            checkpoint_revision: 1,
            turn: 1,
            usage: BudgetUsage::default(),
            complete: false,
            items: Vec::new(),
        }
    }

    #[test]
    fn transcript_binding_uses_surface_fence_not_registry_publication_counter() {
        let task_id = surface::SurfaceTaskId::try_new("child").expect("task id");
        let task = surface_task("child", Some("parent"), 7);
        // Lease acquisition and relay/checkpoint writes advance this counter
        // independently of the actor-owned SurfaceTask revision.
        let record = transcript_record("child", Some("parent"), 23);

        assert!(task_transcript_record_matches_surface_task(
            &task_id, &task, &record
        ));
    }

    #[test]
    fn transcript_binding_rejects_task_or_parent_mismatch() {
        let task_id = surface::SurfaceTaskId::try_new("child").expect("task id");
        let task = surface_task("child", Some("parent"), 1);

        assert!(!task_transcript_record_matches_surface_task(
            &task_id,
            &task,
            &transcript_record("other", Some("parent"), 1),
        ));
        assert!(!task_transcript_record_matches_surface_task(
            &task_id,
            &task,
            &transcript_record("child", Some("other-parent"), 1),
        ));

        assert!(!task_transcript_record_matches_surface_task(
            &task_id,
            &surface_task("other-child", Some("parent"), 1),
            &transcript_record("child", Some("parent"), 1),
        ));
    }
}
