// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

impl ThreadActor {
    pub(super) fn next_surface_transition_retry_at(&self) -> Option<tokio::time::Instant> {
        self.resident_surface
            .0
            .as_ref()
            .into_iter()
            .flat_map(|resident| {
                resident
                    .commit
                    .next_retry_at()
                    .into_iter()
                    .chain(
                        self.background_controller
                            .task_ownership()
                            .into_iter()
                            .map(|pending| pending.retry_at),
                    )
                    .chain(resident.interactions.values().filter_map(|interaction| {
                        interaction
                            .private_response
                            .as_ref()
                            .and_then(|private| private.retry_at)
                    }))
                    .chain(
                        resident
                            .interactions
                            .pending_detaches
                            .values()
                            .map(|pending| pending.retry_at),
                    )
                    .chain(
                        resident
                            .interactions
                            .pending_capability_losses
                            .values()
                            .map(|pending| pending.retry_at),
                    )
                    .chain(resident.capability.pending_transition_retry_times())
            })
            .chain(
                self.operation_recovery
                    .pending_manual_compaction
                    .iter()
                    .map(|pending| pending.retry_at),
            )
            .chain(
                self.goal_controller
                    .pending_recovery()
                    .into_iter()
                    .map(|pending| pending.retry_at),
            )
            .chain(
                self.operation_recovery
                    .pending_provider_transfer
                    .iter()
                    .map(|pending| pending.retry_at),
            )
            .chain(self.background_controller.next_retry_at().into_iter())
            .min()
    }

    pub(super) fn has_pending_surface_transition_retry(&self) -> bool {
        self.next_surface_transition_retry_at().is_some()
    }

    pub(super) fn has_pending_goal_completion_recovery_owner(&self) -> bool {
        self.goal_controller.pending_recovery().is_some()
            || self
                .resident_surface
                .0
                .as_ref()
                .into_iter()
                .any(|resident| resident.commit.has_goal_recovery_owner())
    }

    pub(super) fn pending_goal_completion_recovery_operation_id(
        &self,
    ) -> Option<surface::SurfaceOperationId> {
        self.goal_controller
            .pending_recovery()
            .and_then(|pending| {
                pending
                    .active
                    .surface_operation
                    .as_ref()
                    .map(|fence| fence.operation_id.clone())
            })
            .or_else(|| {
                self.resident_surface
                    .0
                    .as_ref()
                    .into_iter()
                    .find_map(|resident| resident.commit.goal_recovery_operation_id())
            })
    }

    pub(super) fn retry_private_surface_interaction(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let (fence, batch, winner_answer) = {
            let interaction = self
                .resident_surface
                .interactions
                .get(interaction_id)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let private = interaction
                .private_response
                .as_ref()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            (
                interaction.record.fence.clone(),
                private
                    .pending_batch
                    .clone()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                private.answer.clone(),
            )
        };
        let commit_result = match batch.events.as_slice().first().map(|event| &event.scope) {
            Some(surface::SurfaceScope::Background {
                fence: background_fence,
            }) => {
                let safe_projection = interaction_safe_projection(&winner_answer);
                self.resident_surface
                    .coordinator
                    .commit_provider_background_interaction_resolution_batch(
                        background_fence.clone(),
                        &safe_projection,
                        &batch,
                    )
            }
            _ => self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence, &batch),
        };
        if commit_result.is_err() {
            if let Some(private) = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
            {
                private.retry_at =
                    Some(tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL);
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.apply_surface_interaction_resolution(interaction_id, &winner_answer);
        Ok(())
    }

    pub(super) fn drain_private_surface_interactions(
        &mut self,
        fence: &surface::SurfaceOperationFence,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut pending = self
            .resident_surface
            .interactions
            .iter()
            .filter_map(|(interaction_id, interaction)| {
                // Detached mailbox settlement is retried by the actor
                // directly after the public interaction commit. Its private
                // batch must not be submitted to the surface ledger a second
                // time while the mailbox write is down.
                (&interaction.record.fence == fence && interaction.winning_receipt.is_none())
                    .then_some(interaction.private_response.as_ref())
                    .flatten()
                    .and_then(|private| {
                        private
                            .pending_batch
                            .as_ref()
                            .map(|batch| (batch.cursor_before.next_seq, interaction_id.clone()))
                    })
            })
            .collect::<Vec<_>>();
        pending.sort();
        for (_, interaction_id) in pending {
            self.retry_private_surface_interaction(&interaction_id)?;
        }
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
        Ok(())
    }

    pub(super) fn retry_surface_admission_repair(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = SurfaceCommitRetryKey::AdmissionRepair(operation_id.clone());
        let Some(SurfaceCommitEffect::AdmissionRepair(pending)) =
            self.resident_surface.commit.begin_attempt(&key)
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_live_generation_stop_disposition_batch(
                pending.fence.clone(),
                operation_id.clone(),
                pending.finalize_intent_id.clone(),
                &pending.batch,
            )
            .is_err()
        {
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionRepair(pending),
                SurfaceCommitResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                ),
            );
            return;
        }
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: pending.finalize_intent_id.clone(),
                    terminal: pending.terminal.clone(),
                    usage: surface::UsageTotals {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_tokens: 0,
                        estimated_cost_usd_micros: 0,
                    },
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                        "admission repair terminal has no verifier proof",
                    ),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(pending.terminal_commit_id.clone()),
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal: pending.terminal.clone(),
            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                "admission repair terminal has no verifier proof",
            ),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = self.resident_surface.coordinator.commit_finalizer_batch(
            operation_id.clone(),
            pending.finalize_intent_id.clone(),
            &terminal_batch,
        ) {
            eprintln!("orca: typed surface admission repair terminal retry failed: {error:?}");
            let finalize_intent_id = pending.finalize_intent_id.clone();
            let terminal_commit_id = pending.terminal_commit_id.clone();
            let repair = surface::RetryFinalizationToken::new(
                pending.original_request_id.clone(),
                pending.fence.thread_id.clone(),
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                pending.fence.thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            self.cache_surface_admission_terminal_failure(PendingSurfaceAdmissionTerminal {
                pending: PendingSurfaceTerminalCommit {
                    batch: terminal_batch,
                    value,
                    failure: surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        operation_id: operation_id.clone(),
                        finalize_intent_id,
                        commit_id: terminal_commit_id,
                        repair,
                    },
                    legacy_completion: pending.legacy_completion.clone(),
                    legacy_terminal: pending.legacy_terminal.clone(),
                },
                goal_recovery_owned: pending.goal_recovery_owned,
                retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
            });
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionRepair(pending),
                SurfaceCommitResolution::Committed,
            );
            return;
        }
        let Some(SurfaceCommitEffect::AdmissionRepair(pending)) =
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionRepair(pending),
                SurfaceCommitResolution::Committed,
            )
        else {
            unreachable!("committed admission repair effect must be returned")
        };
        self.cache_surface_terminal(value);
        if let (Some(completion), Some(terminal)) =
            (pending.legacy_completion, pending.legacy_terminal)
        {
            self.goal_controller.clear_active(terminal.operation_id);
            let completed = completion.complete(terminal);
            debug_assert!(completed, "legacy terminal must complete exactly once");
        }
        if self.resident_surface.commit.pending_terminals_empty() {
            self.operation_recovery.terminal_blocked = None;
        }
    }

    pub(super) fn retry_surface_admission_commit(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = SurfaceCommitRetryKey::AdmissionCommit(operation_id.clone());
        let Some(SurfaceCommitEffect::AdmissionCommit(pending)) =
            self.resident_surface.commit.begin_attempt(&key)
        else {
            return;
        };
        let commit = match pending.goal.as_ref() {
            Some(goal) => self.resident_surface.coordinator.commit_actor_goal_batch(
                goal.goal_fence.clone(),
                goal.receipt_digest.clone(),
                &pending.batch,
            ),
            None => self
                .resident_surface
                .coordinator
                .commit_actor_batch(&pending.batch),
        };
        if commit.is_err() {
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionCommit(pending),
                SurfaceCommitResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                ),
            );
            return;
        }
        let Some(SurfaceCommitEffect::AdmissionCommit(pending)) =
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionCommit(pending),
                SurfaceCommitResolution::Committed,
            )
        else {
            unreachable!("committed admission commit effect must be returned")
        };
        if let Some(goal) = pending.goal.as_ref() {
            Self::schedule_goal_surface_acknowledgement(
                goal.runtime.clone(),
                goal.mutation.clone(),
            );
        }
        if let Err(error) = self.repair_surface_admission_failure(&pending.fence, pending.message) {
            self.operation_recovery.terminal_blocked = Some(format!(
                "typed surface admission repair failed for {:?}: {error:?}",
                pending.fence.operation_id
            ));
        }
    }

    pub(super) fn retry_surface_admission_terminal(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = SurfaceCommitRetryKey::AdmissionTerminal(operation_id.clone());
        let Some(SurfaceCommitEffect::AdmissionTerminal(pending)) =
            self.resident_surface.commit.begin_attempt(&key)
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                match &pending.pending.failure {
                    surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        finalize_intent_id,
                        ..
                    } => finalize_intent_id.clone(),
                    _ => return,
                },
                &pending.pending.batch,
            )
            .is_err()
        {
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionTerminal(pending),
                SurfaceCommitResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                ),
            );
            return;
        }
        let Some(SurfaceCommitEffect::AdmissionTerminal(pending)) =
            self.resident_surface.commit.resolve_attempt(
                SurfaceCommitEffect::AdmissionTerminal(pending),
                SurfaceCommitResolution::Committed,
            )
        else {
            unreachable!("committed admission terminal effect must be returned")
        };
        let PendingSurfaceAdmissionTerminal {
            pending: terminal_pending,
            ..
        } = pending;
        self.cache_surface_terminal(terminal_pending.value);
        if let (Some(completion), Some(terminal)) = (
            terminal_pending.legacy_completion,
            terminal_pending.legacy_terminal,
        ) {
            self.goal_controller.clear_active(terminal.operation_id);
            let completed = completion.complete(terminal);
            debug_assert!(completed, "legacy terminal must complete exactly once");
        }
        if self.resident_surface.commit.pending_terminals_empty() {
            self.operation_recovery.terminal_blocked = None;
        }
    }

    pub(super) fn mirror_surface_task_ownership(pending: &PendingSurfaceTaskOwnership) {
        let Some(task) = pending.task_registry.get(pending.task_id.as_str()) else {
            eprintln!(
                "orca: durable task ownership could not find legacy task {}",
                pending.task_id.as_str()
            );
            return;
        };
        let result = match (pending.backgrounded, task.is_backgrounded) {
            (true, false) => pending
                .task_registry
                .mark_backgrounded(pending.task_id.as_str()),
            (false, true) if task.status == TaskStatus::ApprovalRequired => pending
                .task_registry
                .reconcile_main_session_backgrounded(pending.task_id.as_str(), false),
            (false, true) => pending
                .task_registry
                .mark_foregrounded(pending.task_id.as_str()),
            _ => Ok(()),
        };
        if let Err(error) = result {
            eprintln!("orca: durable task ownership outpaced legacy registry persistence: {error}");
        }
    }

    pub(super) fn retry_surface_task_ownership(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let Some(mut pending) = self.background_controller.take_task_ownership() else {
            return;
        };
        if &pending.operation_id != operation_id {
            self.background_controller.retain_task_ownership(pending);
            return;
        }
        if self
            .resident_surface
            .coordinator
            .commit_actor_batch(&pending.batch)
            .is_err()
        {
            pending.retry_at = tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            self.background_controller.retain_task_ownership(pending);
            return;
        }
        Self::mirror_surface_task_ownership(&pending);
    }

    pub(super) fn retry_pending_surface_transition(
        &mut self,
        mut active: Option<&mut ActiveOperation>,
    ) {
        let manual_compaction = self
            .operation_recovery
            .pending_manual_compaction
            .iter()
            .map(|pending| {
                (
                    pending.retry_at,
                    PendingSurfaceTransitionRetry::ManualCompactionCompletion,
                )
            });
        let goal_completion = self
            .goal_controller
            .pending_recovery()
            .into_iter()
            .map(|pending| {
                (
                    pending.retry_at,
                    PendingSurfaceTransitionRetry::GoalCompletionRecovery,
                )
            });
        let provider_transfer = self
            .operation_recovery
            .pending_provider_transfer
            .iter()
            .map(|pending| {
                (
                    pending.retry_at,
                    PendingSurfaceTransitionRetry::ProviderTransfer(
                        pending.fence.operation_id.clone(),
                    ),
                )
            });
        let background_retries =
            self.background_controller
                .next_retry()
                .into_iter()
                .map(|(retry_at, key)| {
                    let retry = match key {
                        BackgroundRetryKey::WorkflowCompletion(operation_id) => {
                            PendingSurfaceTransitionRetry::WorkflowCompletion(operation_id)
                        }
                        BackgroundRetryKey::ProviderPreparation(operation_id) => {
                            PendingSurfaceTransitionRetry::ProviderPreparation(operation_id)
                        }
                        BackgroundRetryKey::ProviderCompletion(operation_id) => {
                            PendingSurfaceTransitionRetry::ProviderCompletion(operation_id)
                        }
                        BackgroundRetryKey::ApprovalResolution(operation_id) => {
                            PendingSurfaceTransitionRetry::BackgroundApprovalResolution(
                                operation_id,
                            )
                        }
                        BackgroundRetryKey::Control(operation_id) => {
                            PendingSurfaceTransitionRetry::BackgroundControl(operation_id)
                        }
                    };
                    (retry_at, retry)
                });
        let background_interaction_routes = self.resident_surface.interactions.iter().filter_map(
            |(interaction_id, interaction)| {
                interaction
                    .pending_background_route
                    .as_ref()
                    .map(|pending| {
                        (
                            pending.retry_at,
                            PendingSurfaceTransitionRetry::BackgroundInteractionRoute(
                                interaction_id.clone(),
                            ),
                        )
                    })
            },
        );
        let task_ownership =
            self.background_controller
                .task_ownership()
                .into_iter()
                .map(|pending| {
                    (
                        pending.retry_at,
                        PendingSurfaceTransitionRetry::TaskOwnership(pending.operation_id.clone()),
                    )
                });
        let commit_retries =
            self.resident_surface
                .commit
                .next_retry()
                .into_iter()
                .map(|(retry_at, key)| {
                    let retry = match key {
                        SurfaceCommitRetryKey::AdmissionCommit(operation_id) => {
                            PendingSurfaceTransitionRetry::AdmissionCommit(operation_id)
                        }
                        SurfaceCommitRetryKey::AdmissionRepair(operation_id) => {
                            PendingSurfaceTransitionRetry::AdmissionRepair(operation_id)
                        }
                        SurfaceCommitRetryKey::AdmissionTerminal(operation_id) => {
                            PendingSurfaceTransitionRetry::AdmissionTerminal(operation_id)
                        }
                        SurfaceCommitRetryKey::Terminalization(operation_id) => {
                            PendingSurfaceTransitionRetry::PreparedTerminalization(operation_id)
                        }
                    };
                    (retry_at, retry)
                });
        let private_responses = self.resident_surface.interactions.iter().filter_map(
            |(interaction_id, interaction)| {
                interaction
                    .private_response
                    .as_ref()
                    .and_then(|private| private.retry_at)
                    .map(|retry_at| {
                        (
                            retry_at,
                            PendingSurfaceTransitionRetry::PrivateResponse(interaction_id.clone()),
                        )
                    })
            },
        );
        let capability_retries = self
            .resident_surface
            .capability
            .pending_transition_retries()
            .map(|(call_id, retry_at)| {
                (
                    retry_at,
                    PendingSurfaceTransitionRetry::CapabilityTransition(call_id),
                )
            });
        let detaches = self
            .resident_surface
            .interactions
            .pending_detaches
            .iter()
            .map(|(attachment_id, pending)| {
                (
                    pending.retry_at,
                    PendingSurfaceTransitionRetry::Detach(attachment_id.clone()),
                )
            });
        let capability_losses = self
            .resident_surface
            .interactions
            .pending_capability_losses
            .iter()
            .map(|(attachment_id, pending)| {
                (
                    pending.retry_at,
                    PendingSurfaceTransitionRetry::CapabilityLoss(attachment_id.clone()),
                )
            });
        let Some((_, retry)) = manual_compaction
            .chain(goal_completion)
            .chain(provider_transfer)
            .chain(background_retries)
            .chain(background_interaction_routes)
            .chain(task_ownership)
            .chain(commit_retries)
            .chain(private_responses)
            .chain(capability_retries)
            .chain(detaches)
            .chain(capability_losses)
            .min()
        else {
            return;
        };
        if retry == PendingSurfaceTransitionRetry::ManualCompactionCompletion {
            let Some(mut pending) = self.operation_recovery.pending_manual_compaction.take() else {
                return;
            };
            let fence = pending
                .active
                .surface_operation
                .clone()
                .expect("pending manual compaction keeps its generation fence");
            if self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence, &pending.batch)
                .is_err()
            {
                pending.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                self.operation_recovery.pending_manual_compaction = Some(pending);
                return;
            }
            pending.active.surface_manual_compaction_committed = true;
            if let Err(error) =
                self.finish_generation(pending.active, Ok(pending.result), pending.allow_resume)
            {
                self.operation_recovery.terminal_blocked = Some(error.to_string());
            }
            return;
        }
        if retry == PendingSurfaceTransitionRetry::GoalCompletionRecovery {
            let Some(pending) = self.goal_controller.take_pending_recovery() else {
                return;
            };
            self.dispatch_surface_goal_completion_recovery(pending.active, pending.message);
            return;
        }
        if let PendingSurfaceTransitionRetry::ProviderTransfer(operation_id) = retry {
            self.retry_typed_provider_transfer(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::WorkflowCompletion(operation_id) = retry {
            self.retry_typed_workflow_completion(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::ProviderPreparation(operation_id) = retry {
            self.retry_typed_provider_preparation(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::ProviderCompletion(operation_id) = retry {
            self.retry_typed_provider_completion(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::BackgroundApprovalResolution(operation_id) = retry {
            let key = BackgroundRetryKey::ApprovalResolution(operation_id);
            let Some(BackgroundRetryEffect::ApprovalResolution {
                operation_id,
                mut pending,
            }) = self.background_controller.begin_retry(&key)
            else {
                return;
            };
            let resolution = if self
                .settle_background_approval_resolution(&mut pending)
                .is_err()
            {
                BackgroundRetryResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                )
            } else {
                BackgroundRetryResolution::Settled
            };
            self.background_controller.resolve_retry(
                BackgroundRetryEffect::ApprovalResolution {
                    operation_id,
                    pending,
                },
                resolution,
            );
            return;
        }
        if let PendingSurfaceTransitionRetry::BackgroundInteractionRoute(interaction_id) = retry {
            self.retry_background_interaction_route(&interaction_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::TaskOwnership(operation_id) = retry {
            self.retry_surface_task_ownership(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::BackgroundControl(operation_id) = retry {
            self.retry_surface_background_control(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::AdmissionCommit(operation_id) = retry {
            self.retry_surface_admission_commit(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::AdmissionRepair(operation_id) = retry {
            self.retry_surface_admission_repair(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::AdmissionTerminal(operation_id) = retry {
            self.retry_surface_admission_terminal(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::PreparedTerminalization(operation_id) = retry {
            let key = SurfaceCommitRetryKey::Terminalization(operation_id.clone());
            let Some(SurfaceCommitEffect::Terminalization(pending)) =
                self.resident_surface.commit.begin_attempt(&key)
            else {
                return;
            };
            debug_assert_eq!(pending.fence.operation_id, operation_id);
            if self
                .resident_surface
                .coordinator
                .commit_actor_generation_terminalization_batch(
                    pending.fence.clone(),
                    &pending.batch,
                )
                .is_err()
            {
                self.resident_surface.commit.resolve_attempt(
                    SurfaceCommitEffect::Terminalization(pending),
                    SurfaceCommitResolution::RetryAt(
                        tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                    ),
                );
                return;
            }
            let Some(SurfaceCommitEffect::Terminalization(pending)) =
                self.resident_surface.commit.resolve_attempt(
                    SurfaceCommitEffect::Terminalization(pending),
                    SurfaceCommitResolution::Committed,
                )
            else {
                unreachable!("committed terminalization effect must be returned")
            };
            if let Some(active) = active.as_deref_mut()
                && active.surface_operation.as_ref() == Some(&pending.fence)
            {
                active.surface_terminalization = Some(pending.cause);
                Self::cancel_active_task_tree(active);
            }
            self.apply_surface_interaction_cancellations(&pending.interaction_ids);
            self.apply_surface_capability_cancellations(&pending.capability_call_ids);
            return;
        }
        if let PendingSurfaceTransitionRetry::PrivateResponse(interaction_id) = retry {
            if self
                .retry_private_surface_interaction(&interaction_id)
                .is_err()
            {
                return;
            }
            self.reconcile_surface_interaction_capabilities(active);
            return;
        }
        if let PendingSurfaceTransitionRetry::CapabilityTransition(call_id) = retry {
            self.retry_surface_capability_transition(&call_id, false);
            return;
        }
        if let PendingSurfaceTransitionRetry::Detach(attachment_id) = retry {
            let pending = self
                .resident_surface
                .interactions
                .pending_detaches
                .get(&attachment_id)
                .expect("selected detach remains pending")
                .clone();
            // Detached permission route transitions are Thread-scoped; all
            // other attachment transitions retain their generation permit.
            let thread_scoped = pending
                .transition
                .batch
                .events
                .as_slice()
                .iter()
                .all(|event| matches!(event.scope, surface::SurfaceScope::Thread));
            let committed = if thread_scoped {
                self.resident_surface
                    .coordinator
                    .commit_actor_batch(&pending.transition.batch)
            } else {
                self.resident_surface.coordinator.commit_generation_batch(
                    pending.transition.fence.clone(),
                    &pending.transition.batch,
                )
            };
            if committed.is_err() {
                if let Some(retained) = self
                    .resident_surface
                    .interactions
                    .pending_detaches
                    .get_mut(&attachment_id)
                {
                    retained.retry_at =
                        tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                }
                return;
            }
            self.resident_surface
                .interactions
                .pending_detaches
                .remove(&attachment_id);
            self.resident_surface
                .interactions
                .pending_capability_losses
                .remove(&attachment_id);
            self.apply_surface_attachment_transition(active.as_deref_mut(), &pending.transition);
            let _ = self
                .resident_surface
                .hub
                .finalize_detach_local(&pending.client, pending.receipt);
            self.reconcile_surface_interaction_capabilities(active);
            return;
        }
        let PendingSurfaceTransitionRetry::CapabilityLoss(attachment_id) = retry else {
            unreachable!("private response and detach retries returned above")
        };
        let transition = self
            .resident_surface
            .interactions
            .pending_capability_losses
            .get(&attachment_id)
            .expect("selected capability loss remains pending")
            .transition
            .clone();
        // Retry with the same authority class selected when the transition
        // was prepared; a detached Thread batch cannot be replayed as a
        // retired generation batch.
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
            if let Some(pending) = self
                .resident_surface
                .interactions
                .pending_capability_losses
                .get_mut(&attachment_id)
            {
                pending.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        self.resident_surface
            .interactions
            .pending_capability_losses
            .remove(&attachment_id);
        self.apply_surface_attachment_transition(active.as_deref_mut(), &transition);
        self.reconcile_surface_interaction_capabilities(active);
    }

    pub(super) fn detach_surface_attachment(
        &mut self,
        mut active: Option<&mut ActiveOperation>,
        client: &surface::RuntimeSurfaceClientHandle,
        request: surface::DetachRequest,
    ) -> surface::DetachResult {
        let attachment_id = client.attachment_id().clone();
        if self.operation_recovery.pending_manual_compaction.is_some() {
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        if !self
            .resident_surface
            .interactions
            .pending_capability_losses
            .is_empty()
            && !self
                .resident_surface
                .interactions
                .pending_capability_losses
                .contains_key(&attachment_id)
        {
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        if !self
            .resident_surface
            .interactions
            .pending_detaches
            .is_empty()
            && !self
                .resident_surface
                .interactions
                .pending_detaches
                .get(&attachment_id)
                .is_some_and(|pending| pending.receipt.request_id == request.request_id)
        {
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        let detached = self
            .resident_surface
            .hub
            .prepare_detach_local(client, request.clone());
        let mut receipt = match detached {
            surface::DetachResult::Detached { receipt } => receipt,
            other => return other,
        };
        let pending = match self
            .resident_surface
            .interactions
            .pending_detaches
            .get(&attachment_id)
            .cloned()
        {
            Some(pending) if pending.receipt.request_id == request.request_id => pending,
            Some(_) => {
                return surface::DetachResult::StaleAttachment {
                    request_id: request.request_id,
                    attachment_id,
                };
            }
            None => {
                let retained_capability_loss = self
                    .resident_surface
                    .interactions
                    .pending_capability_losses
                    .get(&attachment_id)
                    .map(|pending| pending.transition.clone());
                let transition = match retained_capability_loss.map_or_else(
                    || self.prepare_surface_attachment_transition(&attachment_id),
                    |transition| Ok(Some(transition)),
                ) {
                    Ok(Some(transition)) => transition,
                    Ok(None) => {
                        return self
                            .resident_surface
                            .hub
                            .finalize_detach_local(client, receipt);
                    }
                    Err(()) => {
                        return surface::DetachResult::StaleAttachment {
                            request_id: request.request_id,
                            attachment_id,
                        };
                    }
                };
                receipt.affected_route_epochs = transition.affected_route_epochs.clone();
                receipt.route_commit_id = Some(transition.commit_id.clone());
                receipt.route_cursor = Some(transition.batch.cursor_after.clone());
                PendingSurfaceDetach {
                    client: client.clone(),
                    transition,
                    receipt,
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                }
            }
        };
        let thread_scoped = pending
            .transition
            .batch
            .events
            .as_slice()
            .iter()
            .all(|event| matches!(event.scope, surface::SurfaceScope::Thread));
        let committed = if thread_scoped {
            self.resident_surface
                .coordinator
                .commit_actor_batch(&pending.transition.batch)
        } else {
            self.resident_surface.coordinator.commit_generation_batch(
                pending.transition.fence.clone(),
                &pending.transition.batch,
            )
        };
        if committed.is_err() {
            let retry_at = tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            self.resident_surface
                .interactions
                .pending_capability_losses
                .remove(&attachment_id);
            let mut pending = pending;
            pending.retry_at = retry_at;
            self.resident_surface
                .interactions
                .pending_detaches
                .insert(attachment_id.clone(), pending);
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        self.resident_surface
            .interactions
            .pending_detaches
            .remove(&attachment_id);
        self.resident_surface
            .interactions
            .pending_capability_losses
            .remove(&attachment_id);
        self.apply_surface_attachment_transition(active.as_deref_mut(), &pending.transition);
        let result = self
            .resident_surface
            .hub
            .finalize_detach_local(client, pending.receipt);
        self.reconcile_surface_interaction_capabilities(active);
        result
    }

    pub(super) fn persist_surface_settings_metadata_if_recorded(
        &self,
        settings: &surface::SurfaceRuntimeSettings,
    ) -> io::Result<()> {
        if matches!(
            self.resident_surface
                .coordinator
                .state()
                .snapshot()
                .thread
                .persistence,
            surface::ThreadPersistence::RecordedCatalogued
        ) {
            persist_surface_settings_metadata(self.handle.thread_id(), settings)
        } else {
            Ok(())
        }
    }

    pub(super) fn reserve_surface_operation(
        &mut self,
        request_id: surface::SurfaceRequestId,
        intent: surface::OperationRequestIntent,
        origin_attachment: surface::SurfaceAttachmentId,
        origin_connection: Option<surface::SurfaceConnectionId>,
    ) -> Result<
        surface::MutationReply<surface::ReservedOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self.operation_recovery.pending_manual_compaction.is_some()
            || !self.resident_surface.commit.pending_terminals_empty()
            || self.resident_surface.commit.has_pending_admission()
            || self.operation_recovery.terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !matches!(
            intent.correlation,
            surface::OperationIngressCorrelation::TuiUser
                | surface::OperationIngressCorrelation::Headless
                | surface::OperationIngressCorrelation::AcpPrompt { .. }
                | surface::OperationIngressCorrelation::JsonlThreadTurn { .. }
                | surface::OperationIngressCorrelation::JsonlStatelessSubmit { .. }
        ) || intent.kind != surface::OperationKind::UserTurn
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if let surface::OperationIngressCorrelation::JsonlThreadTurn { legacy_turn_id, .. } =
            &intent.correlation
        {
            if TurnId::parse(legacy_turn_id.0.as_str()).is_err() {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
        }
        let mut snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        if !snapshot.session_health.accepting_admission {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if matches!(
            &snapshot.thread.persistence,
            surface::ThreadPersistence::EphemeralNonCataloguedOneShot { .. }
        ) && (snapshot.foreground_operation.is_some()
            || !snapshot.queued_operations.is_empty()
            || !snapshot.operation_history.is_empty())
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let (input_request, non_replayable_reason) =
            match (&intent.replayability, intent.input.as_ref()) {
                (surface::ReplayabilityRequest::CaptureReplayableCapsule, Some(input))
                    if resolve_surface_input(input).is_some() =>
                {
                    (input.clone(), None)
                }
                (surface::ReplayabilityRequest::NonReplayable { reason }, Some(input))
                    if matches!(
                        &snapshot.thread.persistence,
                        surface::ThreadPersistence::EphemeralNonCataloguedOneShot { .. }
                            | surface::ThreadPersistence::EphemeralAttached
                    ) && *reason == surface::NonReplayableReason::HistoryDisabled
                        && resolve_surface_input(input).is_some() =>
                {
                    (input.clone(), Some(*reason))
                }
                _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            };
        if matches!(
            intent.correlation,
            surface::OperationIngressCorrelation::AcpPrompt { .. }
        ) && input_request.blocks.as_slice().iter().any(|block| {
            matches!(
                block,
                surface::SurfaceInputRequestBlock::ResourceLink { .. }
            )
        }) {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: None,
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::UnsupportedContent,
                        message: surface::DisplayText::new(
                            "ACP resource links require a runtime-owned read capability route",
                        ),
                        winning_request_id: None,
                        current_revision: None,
                    }),
                },
            });
        }
        let settings = &snapshot.settings;
        let (expected_settings_revision, expected_policy_epoch) = match &intent.settings_preparation
        {
            surface::OperationSettingsPreparation::UseCurrent {
                expected_settings_revision,
                expected_policy_epoch,
            }
            | surface::OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
                expected_settings_revision,
                expected_policy_epoch,
                ..
            } => (*expected_settings_revision, *expected_policy_epoch),
        };
        let stale_settings_message = if expected_settings_revision != settings.thread_revision {
            Some("thread settings revision is stale")
        } else if expected_policy_epoch != settings.effective.policy_epoch {
            Some("thread settings policy epoch is stale")
        } else {
            None
        };
        if let Some(message) = stale_settings_message {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::RuntimeSettings {
                        host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        thread_id: Some(snapshot.thread.thread_id.clone()),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new(message),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Settings {
                            host_incarnation: self
                                .resident_surface
                                .hub
                                .authority()
                                .host_incarnation()
                                .clone(),
                            thread_id: Some(snapshot.thread.thread_id.clone()),
                            revision: settings.thread_revision,
                        }),
                    }),
                },
            });
        }
        let settings_receipt =
            match &intent.settings_preparation {
                surface::OperationSettingsPreparation::UseCurrent { .. } => {
                    surface::OperationSettingsPreparationReceipt::Current {
                        settings_revision: settings.thread_revision,
                        policy_epoch: settings.effective.policy_epoch,
                    }
                }
                surface::OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
                    patches,
                    ..
                } => {
                    let previous_settings_revision = settings.thread_revision;
                    let mut next_settings = settings.clone();
                    let mut next_config = self.config.clone();
                    for patch in patches.as_slice() {
                        apply_runtime_settings_patch(
                            &mut next_config,
                            &mut next_settings.effective,
                            patch,
                        )?;
                    }
                    next_settings.thread_revision = surface::SettingsRevision::try_new(
                        previous_settings_revision
                            .get()
                            .checked_add(1)
                            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    )
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                    if patches
                        .as_slice()
                        .iter()
                        .any(runtime_settings_patch_affects_policy)
                    {
                        next_settings.effective.policy_epoch =
                            surface::PolicyEpoch::try_new(
                                settings.effective.policy_epoch.get().checked_add(1).ok_or(
                                    surface::SurfaceClientCommandError::RuntimeUnavailable,
                                )?,
                            )
                            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                    }
                    next_settings.pending = None;
                    let host_commit_id =
                        surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                            .expect("generated UUID is v7");
                    let settings_batch = self.surface_event_batch_with_commit_id(
                        vec![(
                            surface::SurfaceScope::Thread,
                            surface::SurfaceEvent::Settings(surface::SettingsPatch::Committed {
                                previous_revision: previous_settings_revision,
                                snapshot: next_settings.clone(),
                            }),
                        )],
                        Some(host_commit_id.clone()),
                    );
                    self.commit_surface_actor_batch_with_retry(&settings_batch)
                        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                    self.config = next_config;
                    self.persist_surface_settings_metadata_if_recorded(&next_settings.effective)
                        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                    let receipt =
                        surface::OperationSettingsPreparationReceipt::ThreadOverridesCommitted {
                            previous_settings_revision,
                            settings_revision: next_settings.thread_revision,
                            policy_epoch: next_settings.effective.policy_epoch,
                            patches_digest: surface_sha256(
                                &serde_json::to_vec(patches.as_slice())
                                    .expect("runtime settings patches are serializable"),
                            ),
                            host_commit_id,
                            thread_settings_cursor: settings_batch.cursor_after.clone(),
                        };
                    snapshot = self.resident_surface.coordinator.state().snapshot().clone();
                    receipt
                }
            };
        self.persist_surface_settings_metadata_if_recorded(&snapshot.settings.effective)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let settings = &snapshot.settings;
        let operation_id =
            surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let reservation_sequence =
            surface::SequenceNumber::new(snapshot.queued_operations.len() as u64 + 1);
        let lease = surface::ReservationLease::new(
            surface::SurfaceAdmissionLeaseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            operation_id.clone(),
            reservation_sequence,
            self.resident_surface
                .hub
                .authority()
                .host_incarnation()
                .clone(),
            surface::MonotonicInstant {
                clock_id: surface::HostMonotonicClockId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                tick: surface::MonotonicTick::new(0),
            },
        );
        if matches!(
            intent.correlation,
            surface::OperationIngressCorrelation::AcpPrompt { .. }
        ) && origin_connection.is_none()
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let jsonl_connection_id = origin_connection.clone().or_else(|| {
            surface::SurfaceConnectionId::try_from_bytes(*origin_attachment.as_bytes()).ok()
        });
        let origin = match &intent.correlation {
            surface::OperationIngressCorrelation::TuiUser => surface::OperationOrigin::TuiUser,
            surface::OperationIngressCorrelation::Headless => surface::OperationOrigin::Headless,
            surface::OperationIngressCorrelation::AcpPrompt {
                session_id,
                inbound_seq,
                rpc_request_id,
            } => surface::OperationOrigin::AcpPrompt {
                connection_id: origin_connection.expect("ACP connection identity is bound"),
                session_id: session_id.clone(),
                inbound_seq: *inbound_seq,
                rpc_request_id: rpc_request_id.clone(),
            },
            surface::OperationIngressCorrelation::JsonlThreadTurn {
                rpc_id_digest,
                legacy_turn_id,
            } => surface::OperationOrigin::JsonlThreadTurn {
                connection_id: jsonl_connection_id
                    .clone()
                    .expect("JSONL connection identity is bound"),
                rpc_id_digest: rpc_id_digest.clone(),
                legacy_turn_id: legacy_turn_id.clone(),
            },
            surface::OperationIngressCorrelation::JsonlStatelessSubmit { rpc_id_digest } => {
                surface::OperationOrigin::JsonlStatelessSubmit {
                    connection_id: jsonl_connection_id.expect("JSONL connection identity is bound"),
                    rpc_id_digest: rpc_id_digest.clone(),
                }
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let replayability = if let Some(reason) = non_replayable_reason {
            surface::Replayability::NonReplayable {
                reason,
                live_capsule: surface::LiveOperationCapsule::Available {
                    incarnation: snapshot.cursor.incarnation.clone(),
                },
            }
        } else {
            surface::Replayability::Replayable {
                capsule_digest: surface_sha256(
                    &serde_json::to_vec(&input_request).expect("surface input is serializable"),
                ),
                request: Some(input_request.clone()),
                request_digest: Some(surface_sha256(
                    &serde_json::to_vec(&input_request).expect("surface input is serializable"),
                )),
                cwd: settings.effective.cwd.clone(),
                workspace_roots: settings.effective.workspace_roots.clone(),
                settings_revision: settings.thread_revision,
                policy_epoch: settings.effective.policy_epoch,
                tool_schema_digest: surface_sha256(
                    &serde_json::to_vec(&snapshot.tools).expect("surface tools are serializable"),
                ),
            }
        };
        let (busy_disposition, interrupt_settlement, legacy_visibility) = match &origin {
            surface::OperationOrigin::JsonlThreadTurn { .. }
            | surface::OperationOrigin::JsonlStatelessSubmit { .. } => (
                surface::BusyDisposition::NotAdmittedImmediately,
                surface::InterruptSettlement::TerminalizeCancelledAtInterruptedStopUnlessResumeQueued,
                surface::LegacyVisibility::JsonlBindingsResolvedBeforeTurnStarted,
            ),
            _ => (
                surface::BusyDisposition::Queue,
                surface::InterruptSettlement::SuspendUntilExplicitControl,
                surface::LegacyVisibility::PublishAfterAdmitted,
            ),
        };
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: request_id.clone(),
            intent: surface::OperationIntent {
                origin,
                kind: intent.kind,
                initial_replayability: replayability,
                busy_disposition,
                interrupt_settlement,
                legacy_visibility,
                settings_revision: settings.thread_revision,
                policy_epoch: settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: crate::runtime_host::surface_capability_fingerprint(
                    &settings.effective,
                    &snapshot.tools,
                ),
                settings_receipt,
            },
            phase: surface::OperationPhase::Requested,
            reservation: lease.clone(),
            ready_for_admission: false,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::Requested { operation }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .insert(operation_id.clone(), origin_attachment);
        if non_replayable_reason.is_some() {
            self.operation_recovery
                .live_input_capsules
                .insert(operation_id.clone(), input_request);
        }
        if matches!(
            snapshot.thread.persistence,
            surface::ThreadPersistence::EphemeralNonCataloguedOneShot { .. }
        ) {
            self.operation_recovery.ephemeral_reservation_expiry =
                Some(EphemeralReservationExpiry {
                    operation_id: operation_id.clone(),
                    expires_at: tokio::time::Instant::now() + self.ephemeral_reservation_timeout,
                });
        }
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &batch,
            surface::ReservedOperationOutput {
                operation_id,
                lease,
                requested_cursor: batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    pub(super) fn manual_compact_surface(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        expected_context_revision: surface::ContextRevision,
    ) -> Result<
        surface::MutationReply<surface::MaintenanceOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if client.grant().role != surface::SurfaceAttachmentRole::Tui {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if let Some(replay) = self.replay_manual_compaction_request(client, request_id.clone())? {
            return Ok(replay);
        }
        if self.active.is_some()
            || self.operation_recovery.pending_manual_compaction.is_some()
            || !self.background_controller.is_empty()
            || !self.resident_surface.interactions.is_empty()
            || !self
                .resident_surface
                .interactions
                .pending_detaches
                .is_empty()
            || !self
                .resident_surface
                .interactions
                .pending_capability_losses
                .is_empty()
            || !self.resident_surface.commit.pending_terminals_empty()
            || self.resident_surface.commit.has_pending_admission()
            || self.operation_recovery.terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.thread.session().has_active_workflows())
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        if snapshot.context.revision != expected_context_revision {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::Thread {
                        thread_id: snapshot.thread.thread_id.clone(),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("context revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Context {
                            thread_id: snapshot.thread.thread_id.clone(),
                            revision: snapshot.context.revision,
                        }),
                    }),
                },
            });
        }
        snapshot
            .context
            .revision
            .get()
            .checked_add(2)
            .and_then(|revision| surface::ContextRevision::try_new(revision).ok())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let operation_id =
            surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let lease = surface::ReservationLease::new(
            surface::SurfaceAdmissionLeaseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            operation_id.clone(),
            surface::SequenceNumber::new(snapshot.queued_operations.len() as u64 + 1),
            self.resident_surface
                .hub
                .authority()
                .host_incarnation()
                .clone(),
            surface::MonotonicInstant {
                clock_id: surface::HostMonotonicClockId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                tick: surface::MonotonicTick::new(0),
            },
        );
        let replayability = surface::Replayability::NonReplayable {
            reason: surface::NonReplayableReason::Missing,
            live_capsule: surface::LiveOperationCapsule::Available {
                incarnation: snapshot.cursor.incarnation.clone(),
            },
        };
        let capability_fingerprint = crate::runtime_host::surface_capability_fingerprint(
            &snapshot.settings.effective,
            &snapshot.tools,
        );
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: request_id.clone(),
            intent: surface::OperationIntent {
                origin: surface::OperationOrigin::TuiUser,
                kind: surface::OperationKind::ManualCompaction {
                    reason: surface::ManualCompactionReason::Manual,
                },
                initial_replayability: replayability.clone(),
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                settings_receipt: surface::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: surface::OperationPhase::Requested,
            reservation: lease,
            ready_for_admission: false,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let requested_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::Requested { operation }],
        );
        self.commit_surface_actor_batch_with_retry(&requested_batch)?;
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .insert(operation_id.clone(), client.attachment_id().clone());

        let logical_turn_id = TurnId::new();
        let fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id: surface::SurfaceGenerationId::new(0),
        };
        let generation = surface::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: surface::GenerationInputState::NotApplicable,
            predecessor: None,
            attempt: surface::GenerationAttempt::Initial,
            goal_identity: None,
            replayability: replayability.clone(),
            required_capabilities: Default::default(),
            capability_fingerprint: capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let admitted_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::Admitted {
                operation_id: operation_id.clone(),
                logical_turn_id,
                input: surface::AdmittedInput::NotApplicable,
                first_generation: generation,
            }],
        );
        if let Err(error) = self.commit_surface_actor_batch_with_retry(&admitted_batch) {
            let _ = self.terminalize_surface_reservation(
                operation_id,
                surface::ReservationFinalizerReason::AdmissionRejected {
                    reason: surface::AdmissionRejectionReason::ConfigurationConflict,
                },
                surface::NotAdmittedReason::ConfigurationConflict,
            );
            return Err(error);
        }

        let start_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: fence.clone(),
                witness: surface::GenerationStartedWitness {
                    started_commit_id: start_commit_id.clone(),
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                    durable_replayability_digest: surface::canonical_replayability_digest(
                        &replayability,
                    ),
                    capability_fingerprint,
                },
            }],
            Some(start_commit_id),
        );
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &started_batch)
            .is_err()
        {
            let _ = self.repair_surface_admission_failure(
                &fence,
                "typed manual compaction start commit failed",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let before_messages = self
            .state
            .as_ref()
            .map(|state| state.thread.session().conversation().messages.len() as u64)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let running_events = self
            .generation_context_controller
            .manual_compaction_running_events(&snapshot, &fence, before_messages)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let running_batch = self.surface_event_batch_with_commit_id(running_events, None);
        if self
            .commit_surface_generation_batch_with_retry(fence.clone(), &running_batch)
            .is_err()
        {
            let _ = self.repair_surface_admission_failure(
                &fence,
                "typed manual compaction running context commit failed",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let precommit_command_tx = self.handle.command_tx.clone();
        let precommit_fence = fence.clone();
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(
                HostedTurnRequest::new("")
                    .with_operation_kind(HostedOperationKind::ManualCompaction)
                    .with_generation_handlers(move |_, _| {
                        let command_tx = precommit_command_tx.clone();
                        let fence = precommit_fence.clone();
                        HostedGenerationHandlers::default().with_manual_compaction_precommit(
                            Arc::new(move |outcome| {
                                let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                                command_tx
                                    .try_send(ThreadCommand::SurfacePrepareManualCompaction {
                                        fence: fence.clone(),
                                        outcome: outcome.clone(),
                                        reply: reply_tx,
                                    })
                                    .map_err(|error| match error {
                                        TrySendError::Full(_) => io::Error::new(
                                            io::ErrorKind::WouldBlock,
                                            "manual compaction precommit mailbox is full",
                                        ),
                                        TrySendError::Closed(_) => io::Error::new(
                                            io::ErrorKind::BrokenPipe,
                                            "manual compaction precommit actor is unavailable",
                                        ),
                                    })?;
                                reply_rx.recv().map_err(|_| {
                                    io::Error::new(
                                        io::ErrorKind::BrokenPipe,
                                        "manual compaction precommit actor closed",
                                    )
                                })?
                            }),
                        )
                    }),
            ),
            writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
            config: None,
            reply: start_tx,
        });
        if !matches!(start_rx.recv(), Ok(Ok(_))) {
            let _ = self.repair_surface_admission_failure(
                &fence,
                "typed manual compaction runtime start failed",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let Some(active) = self.active.as_mut() else {
            let _ = self.repair_surface_admission_failure(
                &fence,
                "typed manual compaction active generation was missing",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        active.surface_operation = Some(fence);
        active.surface_manual_compaction_before_messages = Some(before_messages);

        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &admitted_batch,
            surface::MaintenanceOperationOutput {
                operation_id,
                admitted_cursor: admitted_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    pub(super) fn rebackground_surface_provider(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        target: &surface::BackgroundTarget,
    ) -> Result<
        Option<surface::MutationReply<surface::TransferBackgroundOutput>>,
        surface::SurfaceClientCommandError,
    > {
        if self.background_controller.task_ownership().is_some()
            || self.resident_surface.coordinator.has_incomplete_batch()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let surface::BackgroundTarget::ActiveGeneration {
            fence: requested_fence,
        } = target
        else {
            return Ok(None);
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let Some(background) = snapshot
            .background_operations
            .iter()
            .find(|background| {
                background.operation_id == requested_fence.operation_id
                    && background.fence.operation_fence == *requested_fence
            })
            .cloned()
        else {
            return Ok(None);
        };
        if !self.bind_surface_operation_controller(client, &background.operation_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let Some(task_id) = background.task_id.as_ref() else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let Some(task) = snapshot
            .tasks
            .iter()
            .find(|task| &task.task_id == task_id)
            .cloned()
        else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        if task.task_type != surface::SurfaceTaskType::MainSession
            || task.status != surface::SurfaceTaskStatus::Running
            || task.backgrounded
            || task.background_fence.is_some()
            || task.parent_operation.as_ref() != Some(&background.operation_id)
        {
            return Ok(None);
        }
        let typed = self
            .background_controller
            .tasks()
            .find_map(|background_task| {
                background_task
                    .typed_provider
                    .as_ref()
                    .filter(|typed| {
                        typed.task_id == task.task_id
                            && typed.fence == background.fence
                            && typed.fence.operation_fence == *requested_fence
                    })
                    .cloned()
            });
        let Some(typed) = typed else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let next_revision = surface::TaskRevision::try_new(
            task.revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::OwnershipChanged {
                    task_id: task.task_id.clone(),
                    expected_revision: task.revision,
                    next_revision,
                    backgrounded: true,
                    background_fence: Some(background.fence.clone()),
                }),
            )],
            None,
        );
        let ownership = PendingSurfaceTaskOwnership {
            operation_id: background.operation_id.clone(),
            task_id: task.task_id.clone(),
            task_registry: typed.task_registry.clone(),
            backgrounded: true,
            batch: batch.clone(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        };
        if self.commit_surface_actor_batch_with_retry(&batch).is_err() {
            self.background_controller.retain_task_ownership(ownership);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Self::mirror_surface_task_ownership(&ownership);
        let event = &batch.events.as_slice()[0];
        let target = surface::MutationTarget::Task {
            thread_id: snapshot.thread.thread_id,
            task_id: task.task_id,
        };
        Ok(Some(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target,
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Task,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("task re-background commit has one acknowledgement"),
            },
            value: surface::TransferBackgroundOutput::HandedOff {
                background_fence: background.fence,
                handoff_cursor: batch.cursor_after,
                waiter: surface::OperationWaiterHandle::new(),
            },
        }))
    }

    fn continue_surface_subagent(
        &mut self,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceTaskFence,
        follow_up: Option<String>,
        control: &str,
    ) -> Result<
        surface::MutationReply<surface::TaskControlOutput>,
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == fence.task_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if task.revision != fence.task_revision
            || task.background_fence != fence.background_owner
            || task.task_type != surface::SurfaceTaskType::Subagent
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let subagent = task
            .subagent_id
            .as_ref()
            .and_then(|id| {
                snapshot
                    .subagents
                    .iter()
                    .find(|child| child.subagent_id == *id)
            })
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let continuation = subagent
            .continuation
            .as_ref()
            .filter(|continuation| continuation.resumable && !continuation.indeterminate)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if subagent.status == surface::SurfaceSubagentStatus::Running {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let registry = self.handle.task_registry();
        let binding = registry
            .detached_subagent_binding(task.task_id.as_str())
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let prompt = follow_up.unwrap_or_else(|| match control {
            "retry" => "Retry from the latest safe checkpoint. Re-check external state before repeating any side effect.".to_string(),
            _ => "Continue from the latest safe checkpoint.".to_string(),
        });
        let tool_request = orca_core::tool_types::ToolRequest {
            id: format!("task-control-{}", uuid::Uuid::new_v4()),
            name: orca_core::tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some(format!("{} ({control})", task.description.as_str())),
            raw_arguments: Some(
                serde_json::json!({
                    "description": format!("{} ({control})", task.description.as_str()),
                    "prompt": prompt,
                    "mode": "async",
                    "resume_from": continuation.continuation_id.as_str(),
                })
                .to_string(),
            ),
        };
        let cwd = self
            .config
            .cwd
            .as_deref()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let activity_ingress = binding
            .parent_fence
            .clone()
            .map(|parent_fence| self.handle.subagent_activity_ingress_for(parent_fence));
        let launched = crate::subagent_async_worker::launch_async_subagent(
            crate::subagent_async_worker::AsyncSubagentLaunchContext {
                config: &self.config,
                cwd,
                tool_request: &tool_request,
                request: crate::subagent::create_subagent_request(&tool_request),
                subagent_depth: 0,
                task_registry: &registry,
                root_task_id: binding.parent_task_id.as_deref(),
                parent_fence: binding.parent_fence,
                activity_ingress,
            },
        );
        let launched_task = launched
            .task
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if launched.result.status != orca_core::tool_types::ToolStatus::Completed {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let surfaced = loop {
            if let Some(binding) = registry
                .detached_subagent_binding(&launched_task.id)
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
            {
                self.drain_detached_subagent_relay(&registry, &binding)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            }
            if let Some(task) = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .tasks
                .iter()
                .find(|task| task.task_id.as_str() == launched_task.id)
                .cloned()
            {
                break task;
            }
            if std::time::Instant::now() >= deadline {
                let _ = registry.request_stop(&launched_task.id);
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let next_revision =
            surface::TaskRevision::try_new(surfaced.revision.get().saturating_add(1))
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                    task_id: surfaced.task_id.clone(),
                    expected_revision: surfaced.revision,
                    next_revision,
                    status: surface::SurfaceTaskStatus::Running,
                    completed_at: None,
                    result: None,
                    error: None,
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        let committed_task = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .tasks
            .iter()
            .find(|task| task.task_id == surfaced.task_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Task {
                    thread_id: snapshot.thread.thread_id,
                    task_id: committed_task.task_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Task,
                        event_id: batch.events.as_slice()[0].event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("child continuation commit has one acknowledgement"),
            },
            value: surface::TaskControlOutput {
                task: committed_task,
                cursor: batch.cursor_after,
            },
        })
    }

    pub(super) fn control_surface_task(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::TaskControlAction,
    ) -> Result<
        surface::MutationReply<surface::TaskControlOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self.background_controller.task_ownership().is_some()
            || self.resident_surface.coordinator.has_incomplete_batch()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let (fence, control) = match action {
            surface::TaskControlAction::Foreground { fence } => (fence, "foreground"),
            surface::TaskControlAction::Stop { fence } => (fence, "stop"),
            surface::TaskControlAction::Resume { fence } => (fence, "resume"),
            surface::TaskControlAction::Retry { fence } => (fence, "retry"),
            surface::TaskControlAction::FollowUp { fence, prompt } => {
                return self.continue_surface_subagent(
                    request_id,
                    fence,
                    Some(prompt.as_str().to_string()),
                    "follow-up",
                );
            }
        };
        if matches!(control, "resume" | "retry") {
            return self.continue_surface_subagent(request_id, fence, None, control);
        }
        let foreground = control == "foreground";
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let Some(task) = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == fence.task_id)
            .cloned()
        else {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(surface::MutationTarget::Task {
                        thread_id: snapshot.thread.thread_id,
                        task_id: fence.task_id,
                    }),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::InvalidRequest,
                        message: surface::DisplayText::new("task does not exist"),
                        winning_request_id: None,
                        current_revision: None,
                    }),
                },
            });
        };
        let target = surface::MutationTarget::Task {
            thread_id: snapshot.thread.thread_id.clone(),
            task_id: task.task_id.clone(),
        };
        let current_revision = Some(surface::SurfaceMutationRevision::Task {
            thread_id: snapshot.thread.thread_id.clone(),
            revision: task.revision,
        });
        if task.revision != fence.task_revision || task.background_fence != fence.background_owner {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(target),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("task ownership fence is stale"),
                        winning_request_id: None,
                        current_revision,
                    }),
                },
            });
        }
        if task.task_type == surface::SurfaceTaskType::Subagent {
            if foreground {
                return Ok(surface::MutationReply::Uncommitted {
                    mutation: surface::UncommittedMutation::Invalid {
                        request_id,
                        target: Some(target),
                        error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                            code: surface::SurfaceMutationErrorCode::IllegalState,
                            message: surface::DisplayText::new(
                                "subagents cannot be foregrounded as main sessions",
                            ),
                            winning_request_id: None,
                            current_revision,
                        }),
                    },
                });
            }
            if matches!(
                task.status,
                surface::SurfaceTaskStatus::Stopped
                    | surface::SurfaceTaskStatus::Completed
                    | surface::SurfaceTaskStatus::Failed
                    | surface::SurfaceTaskStatus::Cancelled
            ) {
                return Ok(surface::MutationReply::Uncommitted {
                    mutation: surface::UncommittedMutation::Invalid {
                        request_id,
                        target: Some(target),
                        error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                            code: surface::SurfaceMutationErrorCode::IllegalState,
                            message: surface::DisplayText::new("subagent task is already terminal"),
                            winning_request_id: None,
                            current_revision,
                        }),
                    },
                });
            }
            let Some(subagent_id) = task.subagent_id.as_ref() else {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            };
            let Some(subagent) = snapshot
                .subagents
                .iter()
                .find(|candidate| candidate.subagent_id == *subagent_id)
                .cloned()
            else {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            };
            if !matches!(
                subagent.owner,
                surface::SurfaceSubagentOwner::DetachedTask { .. }
            ) || subagent.status != surface::SurfaceSubagentStatus::Running
            {
                return Ok(surface::MutationReply::Uncommitted {
                    mutation: surface::UncommittedMutation::Invalid {
                        request_id,
                        target: Some(target),
                        error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                            code: surface::SurfaceMutationErrorCode::IllegalState,
                            message: surface::DisplayText::new(
                                "only a running detached subagent can be stopped independently",
                            ),
                            winning_request_id: None,
                            current_revision,
                        }),
                    },
                });
            }

            let task_registry = self.handle.task_registry();
            task_registry
                .request_stop(task.task_id.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let stopped_record = task_registry
                .get(task.task_id.as_str())
                .filter(|record| record.status == TaskStatus::Stopped)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let completed_at = surface::UnixMillis::new(
                stopped_record
                    .completed_at_ms
                    .unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            );
            let next_task_revision = surface::TaskRevision::try_new(
                task.revision
                    .get()
                    .checked_add(1)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let next_subagent_revision = surface::SubagentRevision::try_new(
                subagent
                    .revision
                    .get()
                    .checked_add(1)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let mut activity_history = subagent.subagent_activity_history.clone();
            orca_core::task_types::append_subagent_activity_history(
                &mut activity_history,
                "stopped by user".to_string(),
                subagent.turn,
                completed_at.get(),
            );
            let batch = self.surface_event_batch_with_commit_id(
                vec![
                    (
                        surface::SurfaceScope::Thread,
                        surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                            task_id: task.task_id.clone(),
                            expected_revision: task.revision,
                            next_revision: next_task_revision,
                            status: surface::SurfaceTaskStatus::Stopped,
                            completed_at: Some(completed_at),
                            result: Some(surface::DisplayText::new(
                                stopped_record.result.as_deref().unwrap_or("Task stopped"),
                            )),
                            error: None,
                        }),
                    ),
                    (
                        surface::SurfaceScope::Thread,
                        surface::SurfaceEvent::Subagent(surface::SubagentPatch::Stopped {
                            subagent_id: subagent.subagent_id.clone(),
                            expected_revision: subagent.revision,
                            next_revision: next_subagent_revision,
                            owner: subagent.owner.clone(),
                            subagent_activity_history: activity_history,
                            // Preserve the last actor-accepted continuation. The registry is
                            // an execution mirror and may be updated by the kill path before
                            // this terminal surface event is committed.
                            continuation: subagent.continuation.clone(),
                        }),
                    ),
                ],
                None,
            );
            self.commit_surface_actor_batch_with_retry(&batch)?;
            let committed_task = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .tasks
                .iter()
                .find(|candidate| candidate.task_id == task.task_id)
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target,
                    disposition: surface::MutationDisposition::Accepted,
                    acknowledgements: surface::NonEmptyVec::try_new(
                        batch
                            .events
                            .as_slice()
                            .iter()
                            .map(|event| surface::MutationCommitAck::ThreadLocalCursor {
                                cursor: batch.cursor_after.clone(),
                                family: match event.event {
                                    surface::SurfaceEvent::Task(_) => {
                                        surface::SurfaceFactFamily::Task
                                    }
                                    surface::SurfaceEvent::Subagent(_) => {
                                        surface::SurfaceFactFamily::Subagent
                                    }
                                    _ => unreachable!("subagent stop batch has two fact families"),
                                },
                                event_id: event.event_id.clone(),
                                commit_class: batch.commit_class.clone(),
                            })
                            .collect(),
                    )
                    .expect("subagent stop commit has task and subagent acknowledgements"),
                },
                value: surface::TaskControlOutput {
                    task: committed_task,
                    cursor: batch.cursor_after,
                },
            });
        }
        if task.task_type != surface::SurfaceTaskType::MainSession
            || matches!(
                task.status,
                surface::SurfaceTaskStatus::Stopped
                    | surface::SurfaceTaskStatus::Completed
                    | surface::SurfaceTaskStatus::Failed
                    | surface::SurfaceTaskStatus::Cancelled
            )
        {
            let message = if foreground {
                "foreground task requires a backgrounded task".to_string()
            } else {
                let status = match task.status {
                    surface::SurfaceTaskStatus::Stopped => "stopped",
                    surface::SurfaceTaskStatus::Completed => "completed",
                    surface::SurfaceTaskStatus::Failed => "failed",
                    surface::SurfaceTaskStatus::Cancelled => "cancelled",
                    _ => "not controllable",
                };
                format!("task is already {status}")
            };
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(target),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::IllegalState,
                        message: surface::DisplayText::new(message),
                        winning_request_id: None,
                        current_revision,
                    }),
                },
            });
        }
        if foreground && (!task.backgrounded || task.background_fence.is_none()) {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(target),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::IllegalState,
                        message: surface::DisplayText::new(
                            "foreground task requires a backgrounded task",
                        ),
                        winning_request_id: None,
                        current_revision,
                    }),
                },
            });
        }
        let Some(parent_operation) = task.parent_operation.as_ref() else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        if !self.bind_surface_operation_controller(client, parent_operation) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let owns_background_provider = self.background_controller.has_provider_matching(|typed| {
            typed.task_id == task.task_id
                && typed.fence.operation_fence.operation_id == *parent_operation
                && (task.background_fence.is_none()
                    || Some(&typed.fence) == task.background_fence.as_ref())
        });
        let owns_suspended_background_approval = foreground
            && task.status == surface::SurfaceTaskStatus::ApprovalRequired
            && task.background_fence.as_ref().is_some_and(|fence| {
                snapshot.background_operations.iter().any(|background| {
                    &background.fence == fence
                        && background.operation_id == *parent_operation
                        && snapshot.operation_history.iter().any(|operation| {
                            operation.operation_id == *parent_operation
                                && matches!(
                                    operation.phase,
                                    surface::OperationPhase::Suspended {
                                        cause: surface::SuspensionCause::ProviderSuspended { .. }
                                    }
                                )
                        })
                })
            });
        if !owns_background_provider && !owns_suspended_background_approval {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !foreground {
            let operation_id = parent_operation.clone();
            let (_, batch) = self.cancel_surface_background_provider_with_batch(
                request_id.clone(),
                operation_id,
                &snapshot,
            )?;
            let committed_task = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .tasks
                .iter()
                .find(|candidate| candidate.task_id == task.task_id)
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let task_event = &batch.events.as_slice()[1];
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target,
                    disposition: surface::MutationDisposition::Accepted,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::ThreadLocalCursor {
                            cursor: batch.cursor_after.clone(),
                            family: surface::SurfaceFactFamily::Task,
                            event_id: task_event.event_id.clone(),
                            commit_class: batch.commit_class.clone(),
                        },
                    ])
                    .expect("task stop commit has one task acknowledgement"),
                },
                value: surface::TaskControlOutput {
                    task: committed_task,
                    cursor: batch.cursor_after,
                },
            });
        }
        let next_revision = surface::TaskRevision::try_new(
            task.revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::OwnershipChanged {
                    task_id: task.task_id.clone(),
                    expected_revision: task.revision,
                    next_revision,
                    backgrounded: false,
                    background_fence: None,
                }),
            )],
            None,
        );
        let task_registry = self
            .state
            .as_ref()
            .map(|state| state.thread.session().task_registry().clone())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if task_registry.get(task.task_id.as_str()).is_none() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let ownership = PendingSurfaceTaskOwnership {
            operation_id: parent_operation.clone(),
            task_id: task.task_id.clone(),
            task_registry,
            backgrounded: false,
            batch: batch.clone(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        };
        if self.commit_surface_actor_batch_with_retry(&batch).is_err() {
            self.background_controller.retain_task_ownership(ownership);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Self::mirror_surface_task_ownership(&ownership);
        let committed_task = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .tasks
            .iter()
            .find(|candidate| candidate.task_id == task.task_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let event = &batch.events.as_slice()[0];
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target,
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Task,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("task ownership commit has one acknowledgement"),
            },
            value: surface::TaskControlOutput {
                task: committed_task,
                cursor: batch.cursor_after,
            },
        })
    }

    pub(super) fn control_surface_workflow(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::WorkflowControlAction,
    ) -> Result<
        surface::MutationReply<surface::WorkflowControlOutput>,
        surface::SurfaceClientCommandError,
    > {
        let (catalog_entry_id, observed_catalog_revision, args, parent) = match action {
            surface::WorkflowControlAction::Launch {
                catalog_entry_id,
                observed_catalog_revision,
                args,
                parent,
            } => (catalog_entry_id, observed_catalog_revision, args, parent),
            surface::WorkflowControlAction::Stop { fence } => {
                return self.stop_surface_workflow(request_id, fence);
            }
            surface::WorkflowControlAction::Pause { .. }
            | surface::WorkflowControlAction::Resume { .. } => {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        };
        if observed_catalog_revision
            != surface::WorkflowCatalogRevision::try_new(1).expect("one is valid")
            || parent.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let mut raw_args = serde_json::Map::new();
        for (name, value) in args {
            let value = serde_json::from_str(value.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            if raw_args.insert(name.as_str().to_string(), value).is_some() {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        }
        let workflow_args = serde_json::Value::Object(raw_args);
        if let Some(replay) = self.replay_surface_workflow_launch(
            client,
            request_id.clone(),
            &catalog_entry_id,
            &workflow_args,
        )? {
            return Ok(replay);
        }
        if self.active.is_some() || self.operation_recovery.pending_manual_compaction.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.ensure_background_capacity(1)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let config = self.config.clone();
        if !config.workflows.enabled {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let cwd = config
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let task_registry = self
            .state
            .as_ref()
            .map(|state| state.thread.session().task_registry().clone())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let session_dir = task_registry
            .workflow_session_dir(&cwd)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let runner = WorkflowRunner::new(config, task_registry.clone(), session_dir);
        let prepared = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                name: Some(catalog_entry_id.as_str().to_string()),
                args: Some(workflow_args.clone()),
                ..Default::default()
            }))
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let tool_use_id = format!("workflow-{}", uuid::Uuid::new_v4());
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let task_id = surface::SurfaceTaskId::try_new(prepared.task_id.clone())
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let workflow_run_id = surface::SurfaceWorkflowRunId::try_new(prepared.run_id.clone())
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let workflow_name = surface::NonEmptyText::try_new(prepared.workflow_name.clone())
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let surface_tool_use_id = surface::SurfaceToolCallId::try_new(tool_use_id.clone())
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;

        let operation_id =
            surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let lease = surface::ReservationLease::new(
            surface::SurfaceAdmissionLeaseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            operation_id.clone(),
            surface::SequenceNumber::new(snapshot.queued_operations.len() as u64 + 1),
            self.resident_surface
                .hub
                .authority()
                .host_incarnation()
                .clone(),
            surface::MonotonicInstant {
                clock_id: surface::HostMonotonicClockId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                tick: surface::MonotonicTick::new(0),
            },
        );
        let replayability = surface::Replayability::NonReplayable {
            reason: surface::NonReplayableReason::Missing,
            live_capsule: surface::LiveOperationCapsule::Available {
                incarnation: snapshot.cursor.incarnation.clone(),
            },
        };
        let capability_fingerprint =
            surface_workflow_launch_fingerprint(&catalog_entry_id, &workflow_args);
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: request_id.clone(),
            intent: surface::OperationIntent {
                origin: surface::OperationOrigin::TuiUser,
                kind: surface::OperationKind::StandaloneWorkflow {
                    workflow: catalog_entry_id,
                },
                initial_replayability: replayability.clone(),
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                settings_receipt: surface::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: surface::OperationPhase::Requested,
            reservation: lease,
            ready_for_admission: false,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let generation_fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id: surface::SurfaceGenerationId::new(0),
        };
        let logical_turn_id = TurnId::new();
        let generation = surface::GenerationRecord {
            fence: generation_fence.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: surface::GenerationInputState::NotApplicable,
            predecessor: None,
            attempt: surface::GenerationAttempt::Initial,
            goal_identity: None,
            replayability: replayability.clone(),
            required_capabilities: Default::default(),
            capability_fingerprint: capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let background_fence = surface::SurfaceBackgroundFence {
            operation_fence: generation_fence.clone(),
            background_owner_token: surface::SurfaceBackgroundOwnerToken::new(random_token_bytes()),
        };
        let task = surface::SurfaceTask {
            task_id: task_id.clone(),
            revision: surface::TaskRevision::try_new(1).expect("one is valid"),
            task_type: surface::SurfaceTaskType::Workflow,
            status: surface::SurfaceTaskStatus::Running,
            backgrounded: true,
            description: surface::DisplayText::new(prepared.workflow_description.clone()),
            created_at: surface::UnixMillis::new(prepared.created_at_ms),
            started_at: Some(surface::UnixMillis::new(prepared.created_at_ms)),
            completed_at: None,
            parent_operation: Some(operation_id.clone()),
            parent_task_id: None,
            background_fence: Some(background_fence.clone()),
            workflow_run_id: Some(workflow_run_id.clone()),
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        };
        let initial_workflow = surface::SurfaceWorkflow {
            workflow_run_id: workflow_run_id.clone(),
            task_id: task_id.clone(),
            revision: surface::WorkflowRevision::try_new(1).expect("one is valid"),
            name: workflow_name,
            status: surface::SurfaceWorkflowStatus::Running,
            phases: Vec::new(),
            agents: Vec::new(),
            result: None,
            error: None,
            parent: None,
        };
        let final_workflow = surface::SurfaceWorkflow {
            revision: surface::WorkflowRevision::try_new(2).expect("two is valid"),
            status: surface::SurfaceWorkflowStatus::AsyncLaunched,
            ..initial_workflow.clone()
        };
        let started_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let events = vec![
            (
                surface::SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::Requested { operation }),
            ),
            (
                surface::SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::Admitted {
                    operation_id: operation_id.clone(),
                    logical_turn_id,
                    input: surface::AdmittedInput::NotApplicable,
                    first_generation: generation,
                }),
            ),
            (
                surface::SurfaceScope::Generation {
                    fence: generation_fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStarted {
                    fence: generation_fence.clone(),
                    witness: surface::GenerationStartedWitness {
                        started_commit_id: started_commit_id.clone(),
                        settings_revision: snapshot.settings.thread_revision,
                        policy_epoch: snapshot.settings.effective.policy_epoch,
                        durable_replayability_digest: surface::canonical_replayability_digest(
                            &replayability,
                        ),
                        capability_fingerprint,
                    },
                }),
            ),
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::Upserted {
                    expected_revision: None,
                    task,
                }),
            ),
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Workflow(surface::WorkflowPatch::Started {
                    workflow: initial_workflow,
                }),
            ),
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Workflow(surface::WorkflowPatch::AsyncLaunched {
                    fence: surface::SurfaceWorkflowFence {
                        workflow_run_id: workflow_run_id.clone(),
                        workflow_revision: surface::WorkflowRevision::try_new(1)
                            .expect("one is valid"),
                        parent: None,
                    },
                    next_revision: surface::WorkflowRevision::try_new(2).expect("two is valid"),
                }),
            ),
            (
                surface::SurfaceScope::Generation {
                    fence: generation_fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationTransferred {
                    fence: generation_fence,
                    background_fence: background_fence.clone(),
                    task_id: Some(task_id.clone()),
                }),
            ),
        ];
        let batch = self.surface_event_batch_with_commit_id(events, Some(started_commit_id));
        let mut launch_committed = false;
        for _ in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            if self
                .resident_surface
                .coordinator
                .commit_actor_batch(&batch)
                .is_ok()
            {
                launch_committed = true;
                break;
            }
        }
        if !launch_committed {
            runner.abort_prepared_background(
                prepared,
                "typed workflow launch was not durably committed".to_string(),
            );
            if self.resident_surface.coordinator.has_incomplete_batch() {
                self.operation_recovery.terminal_blocked =
                    Some("typed workflow launch commit is retained for cold recovery".to_string());
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .insert(operation_id.clone(), client.attachment_id().clone());
        let typed_workflow = TypedWorkflowBackground {
            fence: background_fence,
            task_id,
            workflow_run_id: workflow_run_id.clone(),
            tool_use_id: surface_tool_use_id,
        };
        let returned_workflow = final_workflow;
        match runner.activate_background(prepared.clone()) {
            Ok(launch) => {
                let events = self
                    .state
                    .as_ref()
                    .map(|state| state.events.fork())
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let launched_task_id = launch.task_id.clone();
                self.spawn_workflow_background_tasks(
                    task_registry,
                    &events,
                    None,
                    RuntimeBackgroundWorkflows::from_vec(vec![BackgroundWorkflowRun::new(
                        launch,
                        Some(tool_use_id),
                    )]),
                );
                assert!(
                    self.background_controller
                        .attach_workflow(&launched_task_id, typed_workflow),
                    "activated workflow background task was registered"
                );
            }
            Err(error) => {
                runner.abort_prepared_background(prepared, error.to_string());
                self.commit_typed_workflow_completion(typed_workflow, None)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            }
        }
        let event = |index: usize, family| surface::MutationCommitAck::ThreadLocalCursor {
            cursor: batch.cursor_after.clone(),
            family,
            event_id: batch.events.as_slice()[index].event_id.clone(),
            commit_class: batch.commit_class.clone(),
        };
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Workflow {
                    thread_id: snapshot.thread.thread_id,
                    workflow_run_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    event(4, surface::SurfaceFactFamily::Workflow),
                    event(3, surface::SurfaceFactFamily::Task),
                    event(6, surface::SurfaceFactFamily::Operation),
                ])
                .expect("workflow launch commits workflow, task, and operation"),
            },
            value: surface::WorkflowControlOutput {
                workflow: returned_workflow,
                operation_id: Some(operation_id),
                cursor: batch.cursor_after,
                waiter: Some(surface::OperationWaiterHandle::new()),
            },
        })
    }

    pub(super) fn stop_surface_workflow(
        &mut self,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceWorkflowFence,
    ) -> Result<
        surface::MutationReply<surface::WorkflowControlOutput>,
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let workflow = snapshot
            .workflows
            .iter()
            .find(|workflow| {
                workflow.workflow_run_id == fence.workflow_run_id
                    && workflow.revision == fence.workflow_revision
                    && workflow.parent == fence.parent
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let operation_id = snapshot
            .tasks
            .iter()
            .find(|task| {
                task.task_id == workflow.task_id
                    && task.workflow_run_id.as_ref() == Some(&workflow.workflow_run_id)
            })
            .and_then(|task| task.background_fence.as_ref())
            .map(|background| background.operation_fence.operation_id.clone())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let reply =
            self.cancel_surface_background_workflow(request_id, operation_id.clone(), &snapshot)?;
        match reply {
            surface::MutationReply::Committed {
                mut mutation,
                value,
            } => {
                let (cursor, waiter) = match value {
                    surface::CancelOperationOutput::CancelledBeforeAdmission { terminal }
                    | surface::CancelOperationOutput::AlreadyTerminal { terminal } => {
                        (terminal.cursor, None)
                    }
                    surface::CancelOperationOutput::Accepted {
                        accepted_cursor,
                        waiter,
                        ..
                    } => (accepted_cursor, Some(waiter)),
                    surface::CancelOperationOutput::FinalizationPending {
                        finalization_cursor,
                        waiter,
                        ..
                    } => (finalization_cursor.cursor, Some(waiter)),
                };
                let workflow = self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .workflows
                    .iter()
                    .find(|workflow| workflow.workflow_run_id == fence.workflow_run_id)
                    .cloned()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                mutation.target = surface::MutationTarget::Workflow {
                    thread_id: snapshot.thread.thread_id,
                    workflow_run_id: workflow.workflow_run_id.clone(),
                };
                Ok(surface::MutationReply::Committed {
                    mutation,
                    value: surface::WorkflowControlOutput {
                        workflow,
                        operation_id: Some(operation_id),
                        cursor,
                        waiter,
                    },
                })
            }
            surface::MutationReply::Deferred { mutation, .. } => {
                Ok(surface::MutationReply::Deferred {
                    mutation,
                    partial: surface::DeferredCommandValue::NoValue,
                })
            }
            surface::MutationReply::Uncommitted { mutation } => {
                Ok(surface::MutationReply::Uncommitted { mutation })
            }
        }
    }

    pub(super) fn replay_surface_workflow_launch(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        catalog_entry_id: &surface::SurfaceCatalogEntryId,
        workflow_args: &serde_json::Value,
    ) -> Result<
        Option<surface::MutationReply<surface::WorkflowControlOutput>>,
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let Some(operation) = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.request_id == request_id)
            .cloned()
        else {
            return Ok(None);
        };
        if !matches!(
            &operation.intent.kind,
            surface::OperationKind::StandaloneWorkflow { workflow }
                if workflow.as_str() == catalog_entry_id.as_str()
        ) {
            return Ok(Some(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(surface::MutationTarget::Operation {
                        thread_id: snapshot.thread.thread_id,
                        operation_id: operation.operation_id,
                    }),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::InvalidRequest,
                        message: surface::DisplayText::new(
                            "request id is already bound to another operation",
                        ),
                        winning_request_id: Some(operation.request_id),
                        current_revision: Some(surface::SurfaceMutationRevision::Thread {
                            cursor: snapshot.cursor,
                        }),
                    }),
                },
            }));
        }
        if let Some(bound) = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&operation.operation_id)
            && bound != client.attachment_id()
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let current_workflow = snapshot
            .workflows
            .iter()
            .find(|workflow| {
                snapshot.tasks.iter().any(|task| {
                    task.task_id == workflow.task_id
                        && task.parent_operation.as_ref() == Some(&operation.operation_id)
                })
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let expected_launch_fingerprint =
            surface_workflow_launch_fingerprint(catalog_entry_id, workflow_args);
        if operation.intent.capability_fingerprint != expected_launch_fingerprint {
            return Ok(Some(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(surface::MutationTarget::Workflow {
                        thread_id: snapshot.thread.thread_id,
                        workflow_run_id: current_workflow.workflow_run_id,
                    }),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::InvalidRequest,
                        message: surface::DisplayText::new(
                            "request id is already bound to different workflow arguments",
                        ),
                        winning_request_id: Some(operation.request_id),
                        current_revision: Some(surface::SurfaceMutationRevision::Thread {
                            cursor: snapshot.cursor,
                        }),
                    }),
                },
            }));
        }
        let recovered = self
            .resident_surface
            .coordinator
            .ledger()
            .recover_batches()
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let launch_batch = recovered
            .committed
            .iter()
            .find(|batch| {
                batch.events.as_slice().iter().any(|envelope| {
                    matches!(
                        &envelope.event,
                        surface::SurfaceEvent::Operation(surface::OperationPatch::Requested {
                            operation: requested,
                        }) if requested.operation_id == operation.operation_id
                    )
                })
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if launch_batch.events.as_slice().len() != 7 {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let launch_workflow = match &launch_batch.events.as_slice()[4].event {
            surface::SurfaceEvent::Workflow(surface::WorkflowPatch::Started { workflow }) => {
                surface::SurfaceWorkflow {
                    revision: surface::WorkflowRevision::try_new(2).expect("two is valid"),
                    status: surface::SurfaceWorkflowStatus::AsyncLaunched,
                    ..workflow.clone()
                }
            }
            _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        };
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .entry(operation.operation_id.clone())
            .or_insert_with(|| client.attachment_id().clone());
        let event = |index: usize, family| surface::MutationCommitAck::ThreadLocalCursor {
            cursor: launch_batch.cursor_after.clone(),
            family,
            event_id: launch_batch.events.as_slice()[index].event_id.clone(),
            commit_class: launch_batch.commit_class.clone(),
        };
        Ok(Some(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Workflow {
                    thread_id: snapshot.thread.thread_id,
                    workflow_run_id: launch_workflow.workflow_run_id.clone(),
                },
                disposition: surface::MutationDisposition::AlreadyApplied,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    event(4, surface::SurfaceFactFamily::Workflow),
                    event(3, surface::SurfaceFactFamily::Task),
                    event(6, surface::SurfaceFactFamily::Operation),
                ])
                .expect("workflow replay acknowledges workflow, task, and operation"),
            },
            value: surface::WorkflowControlOutput {
                workflow: launch_workflow,
                operation_id: Some(operation.operation_id),
                cursor: launch_batch.cursor_after,
                waiter: Some(surface::OperationWaiterHandle::new()),
            },
        }))
    }

    pub(super) fn replay_manual_compaction_request(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
    ) -> Result<
        Option<surface::MutationReply<surface::MaintenanceOperationOutput>>,
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let Some(operation) = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.request_id == request_id)
            .cloned()
        else {
            return Ok(None);
        };
        if !matches!(
            operation.intent.kind,
            surface::OperationKind::ManualCompaction {
                reason: surface::ManualCompactionReason::Manual
            }
        ) {
            return Ok(Some(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(surface::MutationTarget::Operation {
                        thread_id: snapshot.thread.thread_id.clone(),
                        operation_id: operation.operation_id,
                    }),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::InvalidRequest,
                        message: surface::DisplayText::new(
                            "request id is already bound to another operation",
                        ),
                        winning_request_id: Some(operation.request_id),
                        current_revision: Some(surface::SurfaceMutationRevision::Thread {
                            cursor: snapshot.cursor.clone(),
                        }),
                    }),
                },
            }));
        }
        if let Some(bound) = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&operation.operation_id)
            && bound != client.attachment_id()
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let recovered = self
            .resident_surface
            .coordinator
            .ledger()
            .recover_batches()
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let admitted_batch = recovered
            .committed
            .iter()
            .find(|batch| {
                batch.events.as_slice().iter().any(|envelope| {
                    matches!(
                        &envelope.event,
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::Admitted { operation_id, .. }
                        ) if operation_id == &operation.operation_id
                    )
                })
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .entry(operation.operation_id.clone())
            .or_insert_with(|| client.attachment_id().clone());
        Ok(Some(Self::committed_surface_mutation(
            request_id,
            operation.operation_id.clone(),
            &admitted_batch,
            surface::MaintenanceOperationOutput {
                operation_id: operation.operation_id,
                admitted_cursor: admitted_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        )))
    }

    pub(super) fn update_surface_session_metadata(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        precondition: surface::SessionMetadataPrecondition,
        patch: surface::SessionMetadataPatch,
    ) -> Result<surface::MutationReply<()>, surface::SurfaceClientCommandError> {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ManageThreadSettings) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let current = snapshot.thread.clone();
        if let surface::SessionMetadataPrecondition::Exact { revision } = precondition
            && revision != current.metadata_revision
        {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::SessionMetadata {
                        thread_id: current.thread_id.clone(),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("session metadata revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Thread {
                            cursor: snapshot.cursor.clone(),
                        }),
                    }),
                },
            });
        }
        let surface::SessionMetadataPatch::SetTitle { title } = patch;
        let next_revision = surface::SessionMetadataRevision::try_new(
            current
                .metadata_revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let updated_at = surface::UnixMillis::new(
            chrono::Utc::now()
                .timestamp_millis()
                .max(current.updated_at.get()),
        );
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Session(surface::SessionPatch::MetadataChanged {
                    previous_revision: current.metadata_revision,
                    next_revision,
                    title: title.clone(),
                    updated_at,
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        let event = &batch.events.as_slice()[0];
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::SessionMetadata {
                    thread_id: current.thread_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Session,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("session metadata commit has one acknowledgement"),
            },
            value: (),
        })
    }

    pub(super) fn update_surface_settings(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        expected_thread_revision: surface::SettingsRevision,
        patches: surface::NonEmptyVec<surface::RuntimeSettingsPatch>,
        active_permission_update: bool,
    ) -> Result<
        surface::MutationReply<surface::SettingsMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ManageThreadSettings) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.operation_recovery.pending_manual_compaction.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let current = snapshot.settings;
        if expected_thread_revision != current.thread_revision {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::RuntimeSettings {
                        host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        thread_id: Some(snapshot.thread.thread_id.clone()),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("thread settings revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Settings {
                            host_incarnation: self
                                .resident_surface
                                .hub
                                .authority()
                                .host_incarnation()
                                .clone(),
                            thread_id: Some(snapshot.thread.thread_id.clone()),
                            revision: current.thread_revision,
                        }),
                    }),
                },
            });
        }
        let mut next_settings = current.clone();
        let mut next_config = self.config.clone();
        for patch in patches.as_slice() {
            apply_runtime_settings_patch(&mut next_config, &mut next_settings.effective, patch)?;
        }
        let active_permission_update_authorized =
            self.resident_surface
                .interactions
                .values()
                .any(|interaction| {
                    interaction.cancelled.is_none()
                        && interaction.winning_receipt.is_none()
                        && interaction_route_admits(&interaction.route, client.attachment_id())
                        && matches!(
                            &interaction.record.request,
                            surface::SurfaceInteractionRequest::PermissionRequest {
                                permissions,
                                ..
                            } if surface_session_permission_settings_delta_authorized(
                                &current.effective,
                                &next_settings.effective,
                                permissions,
                            )
                        )
                });
        if active_permission_update && !active_permission_update_authorized {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let next_revision = surface::SettingsRevision::try_new(
            current
                .thread_revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        next_settings.thread_revision = next_revision;
        if patches
            .as_slice()
            .iter()
            .any(runtime_settings_patch_affects_policy)
        {
            next_settings.effective.policy_epoch = surface::PolicyEpoch::try_new(
                current
                    .effective
                    .policy_epoch
                    .get()
                    .checked_add(1)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        }
        next_settings.pending = None;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Settings(surface::SettingsPatch::Committed {
                    previous_revision: current.thread_revision,
                    snapshot: next_settings.clone(),
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        if let Some(state) = self.state.as_mut() {
            if patches
                .as_slice()
                .iter()
                .any(|patch| matches!(patch, surface::RuntimeSettingsPatch::SetModel { .. }))
            {
                state
                    .thread
                    .session_mut()
                    .set_model(next_config.model.as_history_value().as_deref());
            }
        }
        self.config = next_config;
        self.persist_surface_settings_metadata_if_recorded(&next_settings.effective)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        Ok(self.committed_settings_mutation(
            request_id,
            &batch,
            surface::SettingsMutationOutput {
                settings: next_settings,
                cursor: batch.cursor_after.clone(),
            },
        ))
    }

    pub(super) fn pinned_context_mutation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::PinnedContextAction,
    ) -> Result<
        surface::MutationReply<surface::PinnedContextMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ManagePinnedContext) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.active.is_some() || self.operation_recovery.pending_manual_compaction.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let surface::PinnedContextAction::Add {
            expected_revision,
            entry,
            memory_receipt: _memory_receipt,
        } = action
        else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let current = &snapshot.pinned_context;
        if expected_revision != current.revision {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::Thread {
                        thread_id: snapshot.thread.thread_id.clone(),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("pinned context revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::PinnedContext {
                            thread_id: snapshot.thread.thread_id.clone(),
                            revision: current.revision,
                        }),
                    }),
                },
            });
        }
        if current.entries.iter().any(|current| current.id == entry.id) {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let next_revision = surface::PinnedContextRevision::try_new(
            current
                .revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::PinnedContext(surface::PinnedContextPatch::Added {
                    previous_revision: current.revision,
                    next_revision,
                    entry: entry.clone(),
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        if let Some(state) = self.state.as_mut() {
            state
                .thread
                .session_mut()
                .add_pinned_context(entry.content.as_str().to_string());
        }
        let next_snapshot = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .pinned_context
            .clone();
        Ok(self.committed_pinned_context_mutation(
            request_id,
            &batch,
            surface::PinnedContextMutationOutput {
                snapshot: next_snapshot,
                cursor: batch.cursor_after.clone(),
            },
        ))
    }

    pub(super) fn prepare_surface_admission(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        output_writer: Option<Box<dyn HostedOperationWriter>>,
    ) -> Result<
        (PreparedSurfaceAdmission, Option<PreparedGoalAdmissionWork>),
        surface::SurfaceClientCommandError,
    > {
        if self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.operation_recovery.pending_manual_compaction.is_some()
            || !self.resident_surface.commit.pending_terminals_empty()
            || self.resident_surface.commit.has_pending_admission()
            || self.operation_recovery.terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let operation = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if operation.reservation.lease_id != admission_lease_id {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !matches!(
            operation.intent.origin,
            surface::OperationOrigin::TuiUser
                | surface::OperationOrigin::Headless
                | surface::OperationOrigin::AcpPrompt { .. }
                | surface::OperationOrigin::JsonlThreadTurn { .. }
                | surface::OperationOrigin::JsonlStatelessSubmit { .. }
        ) || !matches!(
            operation.intent.kind,
            surface::OperationKind::UserTurn | surface::OperationKind::GoalRun { .. }
        ) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let (input_request, request_digest, live_capsule_incarnation) =
            match &operation.intent.initial_replayability {
                surface::Replayability::Replayable {
                    request: Some(request),
                    request_digest: Some(request_digest),
                    ..
                } => (request.clone(), Some(*request_digest), None),
                surface::Replayability::NonReplayable {
                    reason: surface::NonReplayableReason::HistoryDisabled,
                    live_capsule: surface::LiveOperationCapsule::Available { incarnation },
                } if incarnation == &snapshot.cursor.incarnation => (
                    self.operation_recovery
                        .live_input_capsules
                        .get(&operation_id)
                        .cloned()
                        .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    None,
                    Some(incarnation.clone()),
                ),
                _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            };
        let resolved_input = resolve_surface_input(&input_request)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let backtrack_target =
            matches!(&operation.intent.origin, surface::OperationOrigin::TuiUser);
        let input_pinned = !backtrack_target;
        let logical_turn_id = match &operation.intent.origin {
            surface::OperationOrigin::JsonlThreadTurn { legacy_turn_id, .. } => {
                TurnId::parse(legacy_turn_id.0.as_str())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
            }
            _ => TurnId::new(),
        };
        let fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id: surface::SurfaceGenerationId::new(0),
        };
        let input_item_id = surface::SurfaceItemId::new();
        let presentation = if live_capsule_incarnation.is_some() {
            surface::SurfaceInputPresentation::Redacted
        } else {
            surface::SurfaceInputPresentation::Visible {
                text: surface_input_presentation_text(&resolved_input),
            }
        };
        let correlation_id =
            surface::SurfaceInputCorrelationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let admitted_input = surface::AdmittedInput::PendingUser {
            item_id: input_item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        };
        let generation_input = surface::GenerationInputState::Pending {
            input_item_id: input_item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        };
        let goal_identity = match &operation.intent.kind {
            surface::OperationKind::UserTurn => None,
            surface::OperationKind::GoalRun {
                goal_id,
                goal_run_id,
                initial_objective_revision,
            } => {
                let goal = snapshot
                    .goal
                    .as_ref()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let run = goal
                    .current_run
                    .as_ref()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                if &goal.goal_id != goal_id
                    || &goal.objective_revision != initial_objective_revision
                    || &run.goal_run_id != goal_run_id
                    || run.operation_id != operation_id
                    || !matches!(run.phase, surface::SurfaceGoalRunPhase::Preparing)
                {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                let outer_turn_origin = match run.run_origin {
                    surface::SurfaceGoalRunOrigin::User => surface::GoalOuterTurnOrigin::User,
                    surface::SurfaceGoalRunOrigin::Resume => surface::GoalOuterTurnOrigin::Resume,
                    surface::SurfaceGoalRunOrigin::WorkflowNotification => {
                        surface::GoalOuterTurnOrigin::WorkflowNotification
                    }
                };
                let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
                Some(surface::SurfaceGoalGenerationIdentity {
                    goal_id: goal_id.clone(),
                    goal_run_id: goal_run_id.clone(),
                    operation_fence: fence.clone(),
                    goal_outer_turn_id: surface::SurfaceGoalOuterTurnId::try_new(
                        outer_turn_id.to_string(),
                    )
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    logical_turn_id: logical_turn_id.clone(),
                    canonical_input_item_id: input_item_id.clone(),
                    outer_turn_origin,
                    attempt: surface::GenerationAttempt::Initial,
                    predecessor_fence: None,
                    objective_revision: *initial_objective_revision,
                    outer_turn_count: 1,
                })
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let generation = surface::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: generation_input,
            predecessor: None,
            attempt: surface::GenerationAttempt::Initial,
            goal_identity: goal_identity.clone(),
            replayability: operation.intent.initial_replayability.clone(),
            required_capabilities: operation.intent.required_capabilities.clone(),
            capability_fingerprint: operation.intent.capability_fingerprint,
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let goal_work = if let Some(identity) = goal_identity.as_ref() {
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let runtime = self
                .state
                .as_ref()
                .and_then(|state| state.thread.initialized_goal_runtime_handle())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let request_digest = request_digest
                .as_ref()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&(
                    "goal_outer_turn_started",
                    operation_id.as_bytes(),
                    identity,
                    request_digest,
                ))
                .expect("Goal outer-turn digest input is serializable"),
            )
            .as_bytes();
            Some(PreparedGoalAdmissionWork {
                runtime,
                input: BeginGoalOuterTurnForSurfaceInput {
                    session_id,
                    expected_goal_id: orca_core::goal_runtime::GoalId::parse(
                        identity.goal_id.as_str(),
                    )
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    expected_goal_revision: u32::try_from(
                        snapshot
                            .goal
                            .as_ref()
                            .expect("Goal identity requires a Goal")
                            .goal_revision
                            .get(),
                    )
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    identity: Box::new(identity.clone()),
                    provider_turn_id: logical_turn_id.to_string(),
                    started_at: chrono::Utc::now().timestamp(),
                },
                context: GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest,
                    goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                },
            })
        } else {
            None
        };
        Ok((
            PreparedSurfaceAdmission {
                request_id,
                operation_id,
                output_writer,
                operation,
                snapshot,
                request_digest,
                live_capsule_incarnation,
                resolved_input,
                backtrack_target,
                input_pinned,
                logical_turn_id,
                fence,
                input_item_id,
                presentation,
                correlation_id,
                admitted_input,
                generation,
                goal_identity,
            },
            goal_work,
        ))
    }

    pub(super) fn dispatch_prepared_surface_admission<Settle>(
        &mut self,
        prepared: PreparedSurfaceAdmission,
        goal_work: Option<PreparedGoalAdmissionWork>,
        settle: Settle,
    ) -> Result<(), RuntimeHostError>
    where
        Settle: FnOnce(
                &mut ThreadActor,
                Result<
                    surface::MutationReply<surface::AdmissionOutput>,
                    surface::SurfaceClientCommandError,
                >,
            ) + Send
            + 'static,
    {
        let Some(goal_work) = goal_work else {
            let result = self.finish_prepared_surface_admission(prepared, None);
            settle(self, result);
            return Ok(());
        };
        self.spawn_goal_blocking(
            "typed Goal outer-turn admission",
            GoalBlockingCompletionKind::PreviewCommit,
            move || prepare_goal_admission_worker(goal_work),
            move |actor, result| {
                let result = result
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)
                    .and_then(|goal_mutation| {
                        actor.finish_prepared_surface_admission(prepared, Some(goal_mutation))
                    });
                settle(actor, result);
            },
        )
    }

    pub(super) fn dispatch_surface_admission_command(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        output_writer: Option<Box<dyn HostedOperationWriter>>,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::AdmissionOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let (prepared, goal_work) = match self.prepare_surface_admission(
            &client,
            request_id,
            operation_id,
            admission_lease_id,
            output_writer,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let dispatched =
            self.dispatch_prepared_surface_admission(prepared, goal_work, move |_actor, result| {
                let _ = reply.send(result);
            });
        if dispatched.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn finish_prepared_surface_admission(
        &mut self,
        prepared: PreparedSurfaceAdmission,
        goal_mutation: Option<(GoalRuntimeHandle, GoalSurfaceMutationRecord)>,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        let PreparedSurfaceAdmission {
            request_id,
            operation_id,
            output_writer,
            operation,
            snapshot,
            request_digest,
            live_capsule_incarnation,
            resolved_input,
            backtrack_target,
            input_pinned,
            logical_turn_id,
            fence,
            input_item_id,
            presentation,
            correlation_id,
            admitted_input,
            generation,
            goal_identity,
        } = prepared;
        let mut admitted_events = vec![
            (
                surface::SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::Admitted {
                    operation_id: operation_id.clone(),
                    logical_turn_id: logical_turn_id.clone(),
                    input: admitted_input,
                    first_generation: generation,
                }),
            ),
            (
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                    item: surface::SurfaceItem::UserMessage {
                        id: input_item_id.clone(),
                        turn_id: logical_turn_id.clone(),
                        input: surface::SurfaceUserInputState::Pending {
                            presentation: presentation.clone(),
                            correlation_id,
                        },
                        pinned: input_pinned,
                        origin: surface::SurfaceItemOrigin::UserInput,
                    },
                }),
            ),
        ];
        let goal_commit_authority = if let Some((_, mutation)) = goal_mutation.as_ref() {
            let (goal_fence, receipt_digest, _, scope, event) =
                surface_goal_mutation_event(mutation, snapshot.thread.thread_id.clone())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            admitted_events.push((scope, event));
            Some((goal_fence, receipt_digest))
        } else {
            None
        };
        let admitted_batch = self.surface_event_batch_with_commit_id(admitted_events, None);
        let mut admitted = false;
        let mut last_error = None;
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            let result = match goal_commit_authority.as_ref() {
                Some((goal_fence, receipt_digest)) => {
                    self.resident_surface.coordinator.commit_actor_goal_batch(
                        goal_fence.clone(),
                        receipt_digest.clone(),
                        &admitted_batch,
                    )
                }
                None => self
                    .resident_surface
                    .coordinator
                    .commit_actor_batch(&admitted_batch),
            };
            match result {
                Ok(_) => {
                    admitted = true;
                    break;
                }
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) =>
                {
                    last_error = Some(surface::SurfaceCommitError::Ledger(error));
                }
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        if !admitted {
            match last_error {
                Some(error) => {
                    eprintln!("orca: typed surface admission commit failed: {error:?}");
                    if matches!(error, surface::SurfaceCommitError::Ledger(_)) {
                        let goal = match (goal_mutation.as_ref(), goal_commit_authority.as_ref()) {
                            (Some((runtime, mutation)), Some((goal_fence, receipt_digest))) => {
                                Some(PendingSurfaceGoalAdmissionCommit {
                                    runtime: runtime.clone(),
                                    mutation: mutation.clone(),
                                    goal_fence: goal_fence.clone(),
                                    receipt_digest: receipt_digest.clone(),
                                })
                            }
                            (None, None) => None,
                            _ => {
                                self.operation_recovery.terminal_blocked = Some(
                                    "typed Goal admission lost its exact commit authority"
                                        .to_string(),
                                );
                                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                            }
                        };
                        self.resident_surface.commit.prepare_admission_commit(
                            PendingSurfaceAdmissionCommit {
                                fence: fence.clone(),
                                batch: admitted_batch,
                                goal,
                                message: "typed surface admission commit failed",
                                retry_at: tokio::time::Instant::now()
                                    + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                            },
                        );
                    } else if let Err(repair_error) = self.repair_surface_admission_failure(
                        &fence,
                        "typed surface admission commit failed",
                    ) {
                        self.operation_recovery.terminal_blocked = Some(format!(
                            "typed surface admission repair failed for {:?}: {repair_error:?}",
                            fence.operation_id
                        ));
                    }
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                None => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            }
        }
        self.clear_ephemeral_reservation_expiry(&operation_id);
        if let Some((runtime, mutation)) = goal_mutation.as_ref() {
            let runtime = runtime.clone();
            let mutation = mutation.clone();
            tokio::task::spawn_blocking(move || {
                Self::acknowledge_goal_surface_mutation_best_effort(&runtime, &mutation);
            });
        }

        let start_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: fence.clone(),
                witness: surface::GenerationStartedWitness {
                    started_commit_id: start_commit_id.clone(),
                    settings_revision: operation.intent.settings_revision,
                    policy_epoch: operation.intent.policy_epoch,
                    durable_replayability_digest: surface::canonical_replayability_digest(
                        &operation.intent.initial_replayability,
                    ),
                    capability_fingerprint: operation.intent.capability_fingerprint.clone(),
                },
            }],
            Some(start_commit_id),
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &started_batch)
        {
            eprintln!("orca: typed surface start commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface generation start commit failed",
            ) {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let resolved_fact = match live_capsule_incarnation {
            Some(live_capsule_incarnation) => surface::SurfaceResolvedInputFact::NonReplayable {
                presentation: surface::SurfaceInputPresentation::Redacted,
                live_capsule_incarnation,
            },
            None => surface::SurfaceResolvedInputFact::Replayable {
                input: surface_input_for_persisted_presentation(&resolved_input),
                request_digest: request_digest
                    .expect("replayable input carries its canonical request digest"),
            },
        };
        let resolved_batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::InputBindingsResolved {
                            fence: fence.clone(),
                            input_item_id: input_item_id.clone(),
                            fact: resolved_fact.clone(),
                        },
                    ),
                ),
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Item(surface::ItemPatch::InputResolved {
                        item_id: input_item_id,
                        fact: resolved_fact,
                    }),
                ),
            ],
            None,
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &resolved_batch)
        {
            eprintln!("orca: typed surface input resolution commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface input resolution commit failed",
            ) {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let legacy_task_id = format!("typed-user-turn-{}", uuid::Uuid::now_v7());
        let loop_started_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn {
                    turn_id: logical_turn_id.clone(),
                    fence: fence.clone(),
                    ordinal: 0,
                    task_id: surface::SurfaceTaskId::try_new(legacy_task_id.clone())
                        .expect("generated task id is non-empty"),
                    task_status: surface::SurfaceTaskRunningStatus::Running,
                },
            }],
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &loop_started_batch)
        {
            eprintln!("orca: typed surface agent-loop start commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface agent-loop start commit failed",
            ) {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = fence.clone();
        let background_control = Arc::new(RuntimeSurfaceBackgroundControl::for_canonical_input(
            resolved_input.canonical_text.as_str(),
        ));
        let generation_background_control = Arc::clone(&background_control);
        #[cfg(test)]
        let provider_input = resolved_input
            .canonical_text
            .as_str()
            .split_whitespace()
            .filter(|token| {
                !token.starts_with("test_surface_suspension_delay_ms=")
                    && !token.starts_with("test_provider_completion_notify_delay_ms=")
            })
            .collect::<Vec<_>>()
            .join(" ");
        #[cfg(not(test))]
        let provider_input = resolved_input.canonical_text.as_str().to_string();
        let images = resolved_input
            .blocks
            .as_slice()
            .iter()
            .filter_map(|block| match block {
                surface::SurfaceInputBlock::Image { source, detail } => {
                    surface_image_input(source, *detail).map(|(image, _)| image)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut hosted_request =
            if matches!(operation.intent.origin, surface::OperationOrigin::Headless) {
                HostedTurnRequest::headless_session(provider_input)
            } else {
                HostedTurnRequest::new(provider_input)
            }
            .with_images(images)
            .with_backtrack_target(backtrack_target)
            .with_task_description(resolved_input.canonical_text.as_str())
            .with_generation_handlers(move |_, cancel| {
                HostedGenerationHandlers::default()
                    .with_provider_suspension_control(generation_background_control.clone())
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
                    .with_acp_read_text_file_handler(Arc::new(RuntimeSurfaceReadTextFileHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_acp_write_text_file_handler(Arc::new(
                        RuntimeSurfaceWriteTextFileHandler {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                        },
                    ))
                    .with_acp_terminal_create_handler(Arc::new(
                        RuntimeSurfaceTerminalCreateHandler {
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
                    .with_mcp_elicitation_handler(Arc::new(RuntimeSurfaceMcpElicitationHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                        cancel,
                    }))
            });
        if goal_identity.is_some() {
            let goal_identity = goal_identity.as_ref().expect("guarded Goal identity");
            let goal_turn_origin = match goal_identity.outer_turn_origin {
                surface::GoalOuterTurnOrigin::User => orca_core::goal_runtime::GoalTurnOrigin::User,
                surface::GoalOuterTurnOrigin::Resume => {
                    orca_core::goal_runtime::GoalTurnOrigin::Resume
                }
                surface::GoalOuterTurnOrigin::Continuation => {
                    orca_core::goal_runtime::GoalTurnOrigin::Continuation
                }
                surface::GoalOuterTurnOrigin::WorkflowNotification => {
                    orca_core::goal_runtime::GoalTurnOrigin::WorkflowNotification
                }
            };
            let surface_goal_turn = crate::goal_actor::GoalTurnContext {
                session_id: self.handle.thread_id().to_string(),
                goal_id: orca_core::goal_runtime::GoalId::parse(goal_identity.goal_id.as_str())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                goal_run_id: orca_core::goal_runtime::GoalRunId::parse(
                    goal_identity.goal_run_id.as_str().to_string(),
                )
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                outer_turn_id: orca_core::goal_runtime::GoalOuterTurnId::parse(
                    goal_identity.goal_outer_turn_id.as_str().to_string(),
                )
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                origin: goal_turn_origin,
                run_started: false,
            };
            hosted_request = hosted_request
                .with_operation_kind(HostedOperationKind::GoalRun)
                .with_goal_tools(true)
                .with_goal_usage_tracking(true)
                .with_goal_turn_origin(goal_turn_origin)
                .with_surface_goal_owned(surface_goal_turn);
        }
        hosted_request.turn_id = logical_turn_id;
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(hosted_request),
            writer: output_writer
                .unwrap_or_else(|| Box::new(PassthroughHostedOperationWriter::new(io::sink()))),
            config: None,
            reply: start_tx,
        });
        let start_result = match start_rx.recv() {
            Ok(result) => result,
            Err(_) => {
                if let Err(repair_error) = self.repair_surface_admission_failure(
                    &fence,
                    "typed surface runtime start reply was dropped",
                ) {
                    self.operation_recovery.terminal_blocked = Some(format!(
                        "typed surface admission repair failed for {:?}: {repair_error:?}",
                        fence.operation_id
                    ));
                }
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        };
        if let Err(error) = start_result {
            eprintln!("orca: typed surface runtime start failed: {error}");
            if let Err(repair_error) =
                self.repair_surface_admission_failure(&fence, "typed surface runtime start failed")
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let Some(active) = self.active.as_mut() else {
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface runtime active generation was missing",
            ) {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        active.surface_operation = Some(fence.clone());
        active.surface_background_control = Some(background_control);

        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &admitted_batch,
            surface::AdmissionOutput::Admitted {
                operation_id,
                first_generation: fence,
                admitted_cursor: admitted_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    pub(super) fn repair_surface_admission_failure(
        &mut self,
        fence: &surface::SurfaceOperationFence,
        message: &'static str,
    ) -> Result<surface::OperationTerminalAtCursor, surface::SurfaceClientCommandError> {
        self.repair_surface_admission_failure_with_legacy(fence, message, None, false)
    }

    pub(super) fn repair_surface_admission_failure_with_legacy(
        &mut self,
        fence: &surface::SurfaceOperationFence,
        message: &'static str,
        legacy: Option<(OperationCompletion, OperationTerminal)>,
        goal_recovery_owned: bool,
    ) -> Result<surface::OperationTerminalAtCursor, surface::SurfaceClientCommandError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == fence.operation_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let original_request_id = operation.request_id.clone();
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let diagnostic = surface::SafeDiagnosticText::try_new(message)
            .expect("admission failure diagnostic is bounded");
        let stop_reason = match operation
            .generations
            .last()
            .map(|generation| generation.phase)
        {
            Some(surface::GenerationPhase::Reserved) => surface::GenerationStopReason::NotStarted {
                reason: surface::NotStartedReason::StartCommitFailure {
                    message: diagnostic.clone(),
                },
            },
            _ => surface::GenerationStopReason::ExecutionFailed {
                class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                message: diagnostic.clone(),
            },
        };
        let stream_discard_reason = surface::AssistantDiscardReason::ProviderFailed;
        let generation_scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == *fence && stream.state == surface::SurfaceAssistantStreamState::Open
            })
            .map(|stream| {
                (
                    generation_scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: stream_discard_reason,
                    }),
                )
            })
            .collect::<Vec<_>>();
        events.push((
            generation_scope,
            surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                fence: fence.clone(),
                reason: stop_reason.clone(),
                usage_delta: surface::UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
            }),
        ));
        events.push((
            surface::SurfaceScope::Operation {
                operation_id: fence.operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                operation_id: fence.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::GenerationStop(
                    stop_reason.clone(),
                ),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        ));
        let stop_and_finalization_batch = self.surface_event_batch_with_commit_id(events, None);
        let terminal = match &stop_reason {
            surface::GenerationStopReason::NotStarted { .. } => {
                surface::OperationTerminal::Failed {
                    class: surface::FailureClass::Persistence,
                    message: diagnostic.clone(),
                }
            }
            _ => surface::OperationTerminal::Failed {
                class: surface::FailureClass::RuntimeInvariant,
                message: diagnostic.clone(),
            },
        };
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_live_generation_stop_disposition_batch(
                fence.clone(),
                fence.operation_id.clone(),
                finalize_intent_id.clone(),
                &stop_and_finalization_batch,
            )
        {
            eprintln!("orca: typed surface admission repair failed: {error:?}");
            self.resident_surface
                .commit
                .prepare_admission_repair(PendingSurfaceAdmissionRepair {
                    fence: fence.clone(),
                    batch: stop_and_finalization_batch,
                    original_request_id,
                    finalize_intent_id,
                    terminal_commit_id,
                    terminal,
                    legacy_completion: legacy.as_ref().map(|(completion, _)| completion.clone()),
                    legacy_terminal: legacy.as_ref().map(|(_, terminal)| terminal.clone()),
                    goal_recovery_owned,
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                });
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let usage = surface::UsageTotals {
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            estimated_cost_usd_micros: 0,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &fence.operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: fence.operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal: terminal.clone(),
                    usage: usage.clone(),
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                        "finalization repair terminal has no verifier proof",
                    ),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id.clone()),
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: fence.operation_id.clone(),
            terminal,
            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                "finalization repair terminal has no verifier proof",
            ),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = self.resident_surface.coordinator.commit_finalizer_batch(
            fence.operation_id.clone(),
            finalize_intent_id.clone(),
            &terminal_batch,
        ) {
            eprintln!("orca: typed surface admission repair terminal failed: {error:?}");
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                snapshot.thread.thread_id.clone(),
                fence.operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                snapshot.thread.owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            self.cache_surface_admission_terminal_failure(PendingSurfaceAdmissionTerminal {
                pending: PendingSurfaceTerminalCommit {
                    batch: terminal_batch,
                    value,
                    failure: surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        operation_id: fence.operation_id.clone(),
                        finalize_intent_id,
                        commit_id: terminal_commit_id,
                        repair,
                    },
                    legacy_completion: legacy.as_ref().map(|(completion, _)| completion.clone()),
                    legacy_terminal: legacy.as_ref().map(|(_, terminal)| terminal.clone()),
                },
                goal_recovery_owned,
                retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
            });
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.cache_surface_terminal(value.clone());
        if let Some((completion, terminal)) = legacy {
            self.goal_controller.clear_active(terminal.operation_id);
            let completed = completion.complete(terminal);
            debug_assert!(completed, "legacy terminal must complete exactly once");
        }
        Ok(value)
    }

    pub(super) fn repair_surface_resume_failure(
        &mut self,
        fence: &surface::SurfaceOperationFence,
        message: &'static str,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == fence.operation_id);
        let repair = operation
            .and_then(|operation| {
                operation
                    .generations
                    .iter()
                    .find(|generation| generation.fence == *fence)
                    .map(|generation| (operation, generation))
            })
            .map(|(operation, generation)| {
                let mut events = snapshot
                    .assistant_streams
                    .iter()
                    .filter(|stream| {
                        stream.fence == *fence
                            && stream.state == surface::SurfaceAssistantStreamState::Open
                    })
                    .map(|stream| {
                        (
                            surface::SurfaceScope::Generation {
                                fence: fence.clone(),
                            },
                            surface::SurfaceEvent::Assistant(
                                surface::AssistantPatch::StreamDiscarded {
                                    stream_id: stream.stream_id.clone(),
                                    reason: surface::AssistantDiscardReason::RuntimeRestart,
                                },
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                match generation.phase {
                    surface::GenerationPhase::Reserved => {
                        let surface::OperationPhase::Suspended { cause } = &operation.phase else {
                            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                        };
                        events.extend([
                            (
                                surface::SurfaceScope::Generation {
                                    fence: fence.clone(),
                                },
                                surface::SurfaceEvent::Operation(
                                    surface::OperationPatch::GenerationStopped {
                                        fence: fence.clone(),
                                        reason: surface::GenerationStopReason::NotStarted {
                                            reason: surface::NotStartedReason::RuntimeRestart,
                                        },
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
                                    operation_id: fence.operation_id.clone(),
                                },
                                surface::SurfaceEvent::Operation(
                                    surface::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                                        operation_id: fence.operation_id.clone(),
                                        previous_cause: cause.clone(),
                                        replacement_fence: fence.clone(),
                                        rebased_cause:
                                            surface::SuspensionCause::RecoveryRequired {
                                                generation_id: fence.generation_id,
                                            },
                                    },
                                ),
                            ),
                        ]);
                    }
                    surface::GenerationPhase::Started | surface::GenerationPhase::Transferred => {
                        events.extend([
                            (
                                surface::SurfaceScope::Generation {
                                    fence: fence.clone(),
                                },
                                surface::SurfaceEvent::Operation(
                                    surface::OperationPatch::GenerationStopped {
                                        fence: fence.clone(),
                                        reason: surface::GenerationStopReason::RuntimeRestart,
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
                                    operation_id: fence.operation_id.clone(),
                                },
                                surface::SurfaceEvent::Operation(
                                    surface::OperationPatch::Suspended {
                                        operation_id: fence.operation_id.clone(),
                                        cause: surface::SuspensionCause::RecoveryRequired {
                                            generation_id: fence.generation_id,
                                        },
                                    },
                                ),
                            ),
                        ]);
                    }
                    surface::GenerationPhase::Stopped => return Ok(()),
                }
                let batch = self.surface_event_batch_with_commit_id(events, None);
                self.resident_surface
                    .coordinator
                    .commit_resume_abort_batch(fence.clone(), &batch)
                    .map(|_| ())
                    .map_err(|error| {
                        eprintln!("orca: {message}: typed surface resume repair failed: {error:?}");
                        surface::SurfaceClientCommandError::RuntimeUnavailable
                    })
            })
            .transpose()
            .map(|_| ());

        if let Some(active) = self.active.as_mut()
            && operation
                .and_then(|operation| {
                    operation
                        .generations
                        .iter()
                        .find(|generation| generation.fence == *fence)
                })
                .map(|generation| generation.logical_turn_id.as_ref())
                .is_some_and(|turn_id: &str| turn_id == active.request.turn_id.as_ref())
        {
            active.generation.cancel.cancel();
        }
        repair
    }

    pub(super) fn dispatch_goal_surface_release(
        &mut self,
        runtime: GoalRuntimeHandle,
        session_id: String,
        identity: surface::SurfaceGoalGenerationIdentity,
    ) {
        let spawned = self.spawn_goal_blocking(
            "typed Goal resume rollback",
            GoalBlockingCompletionKind::Recovery,
            move || {
                runtime
                    .release_outer_turn_for_surface(&session_id, identity)
                    .map_err(|error| RuntimeHostError::GoalControlFailed {
                        message: error.to_string(),
                    })
            },
            |actor, result| match result {
                Ok(true) => {}
                Ok(false) => {
                    actor.operation_recovery.terminal_blocked = Some(
                        "typed Goal resume rollback no longer matched its restored turn"
                            .to_string(),
                    );
                }
                Err(error) => actor.operation_recovery.terminal_blocked = Some(error.to_string()),
            },
        );
        if let Err(error) = spawned {
            self.operation_recovery.terminal_blocked = Some(error.to_string());
        }
    }

    pub(super) fn dispatch_surface_resume<Settle>(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        expected_last_generation: surface::SurfaceGenerationId,
        resume_source: surface::ResumeSourceWitness,
        settle: Settle,
    ) where
        Settle: FnOnce(
                &mut ThreadActor,
                Result<
                    surface::MutationReply<surface::ResumeOperationOutput>,
                    surface::SurfaceClientCommandError,
                >,
            ) + Send
            + 'static,
    {
        let attempt = self.resume_surface_operation(
            &client,
            request_id.clone(),
            operation_id.clone(),
            expected_last_generation,
            resume_source.clone(),
            None,
        );
        let required = match attempt {
            Ok(SurfaceResumeAttempt::Completed(reply)) => {
                settle(self, Ok(reply));
                return;
            }
            Ok(SurfaceResumeAttempt::GoalRestoreRequired {
                runtime,
                session_id,
                identity,
            }) => (runtime, session_id, identity),
            Err(error) => {
                settle(self, Err(error));
                return;
            }
        };
        let (runtime, session_id, identity) = required;
        if self.goal_controller.is_blocking() {
            settle(
                self,
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            );
            return;
        }
        let spawned = self.spawn_goal_blocking(
            "typed Goal resume restore",
            GoalBlockingCompletionKind::PauseResume,
            move || restore_goal_surface_binding_worker(runtime, session_id, identity),
            move |actor, restored| {
                let result = restored
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)
                    .and_then(|restored| {
                        let release = (
                            restored.runtime.clone(),
                            restored.session_id.clone(),
                            restored.identity.clone(),
                        );
                        let attempt = actor.resume_surface_operation(
                            &client,
                            request_id,
                            operation_id,
                            expected_last_generation,
                            resume_source,
                            Some(restored),
                        );
                        match attempt {
                            Ok(SurfaceResumeAttempt::Completed(reply)) => Ok(reply),
                            Ok(SurfaceResumeAttempt::GoalRestoreRequired { .. }) => {
                                actor
                                    .dispatch_goal_surface_release(release.0, release.1, release.2);
                                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
                            }
                            Err(error) => {
                                actor
                                    .dispatch_goal_surface_release(release.0, release.1, release.2);
                                Err(error)
                            }
                        }
                    });
                settle(actor, result);
            },
        );
        debug_assert!(spawned.is_ok(), "Goal resume worker was prevalidated");
    }

    pub(super) fn wait_surface_operation(
        &mut self,
        _request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        caller_cancel: surface::OptionalProcessLocalCancel,
        reply: SyncSender<
            Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        >,
    ) {
        if let Some(value) = self.resident_surface.commit.terminal(&operation_id) {
            let _ = reply.send(Ok(surface::WaitOperationTerminalResult::Terminal {
                value: value.clone(),
            }));
            return;
        }
        if let Some(pending) = self.resident_surface.commit.pending_terminal(&operation_id) {
            let _ = reply.try_send(Ok(pending.failure.clone()));
            return;
        }
        if let Some(pending) = self
            .resident_surface
            .commit
            .admission_terminal(&operation_id)
        {
            let _ = reply.try_send(Ok(pending.pending.failure.clone()));
            return;
        }
        let exists = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .foreground_operation
            .iter()
            .chain(
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .queued_operations
                    .iter(),
            )
            .chain(
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .operation_history
                    .iter(),
            )
            .any(|operation| operation.operation_id == operation_id);
        if !exists {
            let _ = reply.send(Ok(surface::WaitOperationTerminalResult::UnknownOperation {
                operation_id,
            }));
            return;
        }
        self.resident_surface
            .commit
            .register_terminal_waiter(operation_id, reply, caller_cancel);
    }

    pub(super) fn cancel_surface_terminal_waiters(&mut self) {
        for effect in self.resident_surface.commit.cancelled_terminal_waiters() {
            apply_runtime_actor_reply_effect(effect);
        }
    }

    pub(super) fn cache_surface_terminal(&mut self, value: surface::OperationTerminalAtCursor) {
        let operation_id = value.operation_id.clone();
        self.clear_ephemeral_reservation_expiry(&operation_id);
        self.operation_recovery
            .live_input_capsules
            .remove(&operation_id);
        let effects = self
            .resident_surface
            .commit
            .cache_terminal(operation_id, value);
        for effect in effects {
            apply_runtime_actor_reply_effect(effect);
        }
    }

    pub(super) fn clear_ephemeral_reservation_expiry(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        if self
            .operation_recovery
            .ephemeral_reservation_expiry
            .as_ref()
            .is_some_and(|expiry| expiry.operation_id == *operation_id)
        {
            self.operation_recovery.ephemeral_reservation_expiry = None;
        }
    }

    pub(super) fn next_ephemeral_reservation_expiry_at(&self) -> Option<tokio::time::Instant> {
        self.operation_recovery
            .ephemeral_reservation_expiry
            .as_ref()
            .map(|expiry| expiry.expires_at)
    }

    pub(super) fn expire_ephemeral_reservation(&mut self) {
        let Some(expiry) = self.operation_recovery.ephemeral_reservation_expiry.take() else {
            return;
        };
        if expiry.expires_at > tokio::time::Instant::now() {
            self.operation_recovery.ephemeral_reservation_expiry = Some(expiry);
            return;
        }
        let still_reserved = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .any(|operation| operation.operation_id == expiry.operation_id);
        if !still_reserved {
            return;
        }
        if self
            .terminalize_surface_reservation(
                expiry.operation_id.clone(),
                surface::ReservationFinalizerReason::ReservationExpired,
                surface::NotAdmittedReason::ReservationExpired,
            )
            .is_err()
            && !self
                .resident_surface
                .commit
                .has_pending_terminal(&expiry.operation_id)
        {
            self.operation_recovery.ephemeral_reservation_expiry =
                Some(EphemeralReservationExpiry {
                    operation_id: expiry.operation_id,
                    expires_at: tokio::time::Instant::now()
                        + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                });
        }
    }

    pub(super) fn cache_surface_terminal_failure(&mut self, pending: PendingSurfaceTerminalCommit) {
        let operation_id = pending.value.operation_id.clone();
        let failure = pending.failure.clone();
        self.operation_recovery.terminal_blocked =
            Some("typed surface terminal commit failed and requires cold recovery".to_string());
        self.resident_surface
            .commit
            .retain_pending_terminal(operation_id.clone(), pending);
        let effects =
            self.resident_surface
                .commit
                .settle_terminal_waiters(&operation_id, Ok(failure), true);
        for effect in effects {
            apply_runtime_actor_reply_effect(effect);
        }
    }

    pub(super) fn cache_surface_admission_terminal_failure(
        &mut self,
        pending: PendingSurfaceAdmissionTerminal,
    ) {
        let operation_id = pending.pending.value.operation_id.clone();
        let failure = pending.pending.failure.clone();
        self.operation_recovery.terminal_blocked =
            Some("typed surface admission terminal commit is retrying".to_string());
        self.resident_surface
            .commit
            .prepare_admission_terminal(pending);
        let effects =
            self.resident_surface
                .commit
                .settle_terminal_waiters(&operation_id, Ok(failure), true);
        for effect in effects {
            apply_runtime_actor_reply_effect(effect);
        }
    }

    pub(super) fn retry_surface_finalization(
        &self,
        token: surface::RetryFinalizationToken,
    ) -> surface::MutationReply<surface::OperationTerminalAtCursor> {
        let exact_pending = self
            .resident_surface
            .commit
            .pending_terminal(token.operation_id())
            .is_some_and(|pending| {
                let exact = matches!(
                    &pending.failure,
                    surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        repair,
                        ..
                    } if repair == &token
                );
                if exact {
                    debug_assert!(pending.batch.batch_digest == pending.value.batch_digest);
                    if let Some(completion) = pending.legacy_completion.as_ref() {
                        debug_assert!(completion.try_terminal().is_none());
                    }
                    if let Some(terminal) = pending.legacy_terminal.as_ref() {
                        let _ = terminal.outcome();
                    }
                }
                exact
            });
        let code = if exact_pending {
            surface::SurfaceMutationErrorCode::IllegalState
        } else {
            surface::SurfaceMutationErrorCode::InvalidRequest
        };
        let message = if exact_pending {
            "durable operation is Finalizing; live retry is not authoritative"
        } else {
            "retry finalization token does not match resident pending terminal commit"
        };
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Invalid {
                request_id: token.request_id().clone(),
                target: Some(surface::MutationTarget::Operation {
                    thread_id: token.thread_id().clone(),
                    operation_id: token.operation_id().clone(),
                }),
                error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: None,
                }),
            },
        }
    }

    pub(super) fn terminalize_surface_reservation(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        finalizer_reason: surface::ReservationFinalizerReason,
        terminal_reason: surface::NotAdmittedReason,
    ) -> Result<
        (
            surface::OperationTerminalAtCursor,
            surface::SurfaceCommitBatch,
        ),
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = snapshot
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let original_request_id = operation.request_id.clone();
        let thread_id = snapshot.thread.thread_id.clone();
        let thread_owner_epoch = snapshot.thread.owner_epoch;
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let finalization_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::Reservation(finalizer_reason),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }],
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                &finalization_batch,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;

        let terminal = surface::OperationTerminal::NotAdmitted {
            reason: terminal_reason,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
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
                        "unadmitted terminal has no verifier proof",
                    ),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id.clone()),
        );
        let terminal_result = self.resident_surface.coordinator.commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &terminal_batch,
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                "unadmitted terminal has no verifier proof",
            ),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if terminal_result.is_err() {
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                thread_id,
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            let failure = surface::WaitOperationTerminalResult::TerminalCommitFailure {
                operation_id,
                finalize_intent_id,
                commit_id: terminal_commit_id,
                repair,
            };
            self.cache_surface_terminal_failure(PendingSurfaceTerminalCommit {
                batch: terminal_batch,
                value,
                failure,
                legacy_completion: None,
                legacy_terminal: None,
            });
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.cache_surface_terminal(value.clone());
        Ok((value, terminal_batch))
    }

    pub(super) fn terminalize_requested_operations_for_shutdown(
        &mut self,
        reason: surface::SurfaceShutdownReason,
    ) -> Result<(), RuntimeHostError> {
        let operation_ids = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .map(|operation| operation.operation_id.clone())
            .collect::<Vec<_>>();
        let (finalizer_reason, terminal_reason) = match reason {
            surface::SurfaceShutdownReason::HostShutdown => (
                surface::ReservationFinalizerReason::HostShutdown,
                surface::NotAdmittedReason::HostShutdown,
            ),
            surface::SurfaceShutdownReason::ThreadClose => (
                surface::ReservationFinalizerReason::ThreadClose,
                surface::NotAdmittedReason::ThreadClose,
            ),
        };
        for operation_id in operation_ids {
            let _ = self
                .terminalize_surface_reservation(
                    operation_id,
                    finalizer_reason.clone(),
                    terminal_reason,
                )
                .map_err(|error| RuntimeHostError::ThreadStartFailed {
                    message: format!(
                        "failed to terminalize typed reservation during shutdown: {error:?}"
                    ),
                })?;
        }
        Ok(())
    }

    pub(super) fn spawn_goal_surface_finalization(
        &mut self,
        work: GoalSurfaceFinalizationWork,
        acknowledgements: Vec<GoalSurfaceMutationRecord>,
        outcome: OperationOutcome,
        runtime_usage: UsageTotals,
        completed_turn: Option<CompletedTurnOutcome>,
    ) -> Result<(), RuntimeHostError> {
        self.spawn_goal_blocking(
            "typed Goal finalization",
            GoalBlockingCompletionKind::FinishVerify,
            move || {
                for mutation in acknowledgements {
                    let acknowledged = work
                        .control
                        .runtime
                        .acknowledge_surface_mutation(
                            &mutation.receipt.store_commit_id,
                            &mutation.receipt.receipt_digest,
                        )
                        .map_err(|error| RuntimeHostError::GoalControlFailed {
                            message: error.to_string(),
                        })?;
                    if !acknowledged {
                        return Err(RuntimeHostError::GoalControlFailed {
                            message:
                                "typed Goal finalization reconciliation rejected its exact receipt"
                                    .to_string(),
                        });
                    }
                }
                prepare_goal_surface_finalization_worker(work)
            },
            move |actor, result| match result {
                Ok(GoalSurfaceFinalizationWorkerResult::Reconcile { work, worker }) => {
                    let acknowledgements = worker.mutations.clone();
                    let reconciliation = worker.mutations.iter().try_for_each(|mutation| {
                        actor
                            .commit_goal_surface_mutation_with_retry(mutation)
                            .map(|_| ())
                    });
                    if let Err(error) = reconciliation {
                        actor.fail_deferred_goal_finalization(error);
                        return;
                    }
                    if let Err(error) = actor.spawn_goal_surface_finalization(
                        work,
                        acknowledgements,
                        outcome,
                        runtime_usage,
                        completed_turn,
                    ) {
                        actor.fail_deferred_goal_finalization(error);
                    }
                }
                Ok(GoalSurfaceFinalizationWorkerResult::Prepared(prepared)) => {
                    actor.finish_deferred_goal_finalization(
                        outcome,
                        runtime_usage,
                        completed_turn,
                        prepared,
                    );
                }
                Err(error) => actor.fail_deferred_goal_finalization(error),
            },
        )
    }

    pub(super) fn fail_deferred_goal_finalization(&mut self, error: RuntimeHostError) {
        let Some(active) = self.active.take() else {
            self.operation_recovery.terminal_blocked = Some(error.to_string());
            return;
        };
        self.dispatch_surface_goal_completion_recovery(active, error.to_string());
    }

    pub(super) fn finish_deferred_goal_finalization(
        &mut self,
        outcome: OperationOutcome,
        runtime_usage: UsageTotals,
        completed_turn: Option<CompletedTurnOutcome>,
        prepared: PreparedGoalSurfaceFinalization,
    ) {
        let Some(active) = self.active.take() else {
            self.operation_recovery.terminal_blocked =
                Some("typed Goal finalization lost its active operation".to_string());
            return;
        };
        match self.finish_surface_operation(
            &active,
            &outcome,
            runtime_usage,
            completed_turn,
            Some(prepared),
        ) {
            Ok(Some(_)) => {
                if let Some(fence) = active.surface_operation.as_ref() {
                    self.generation_context_controller
                        .clear_operation(&fence.operation_id);
                }
                self.goal_controller.clear_active(active.operation_id);
                let completed = active.completion.complete(OperationTerminal {
                    operation_id: active.operation_id,
                    outcome,
                });
                debug_assert!(completed, "operation terminal must complete exactly once");
            }
            Ok(None) => {
                self.active = Some(active);
                self.operation_recovery.terminal_blocked =
                    Some("typed Goal finalization unexpectedly dispatched twice".to_string());
            }
            Err(error) => {
                let operation_id = active
                    .surface_operation
                    .as_ref()
                    .map(|fence| fence.operation_id.clone())
                    .expect("deferred Goal finalization keeps its operation");
                if self
                    .resident_surface
                    .commit
                    .has_pending_terminal(&operation_id)
                {
                    return;
                }
                self.dispatch_surface_goal_completion_recovery(active, error.to_string());
            }
        }
    }

    pub(super) fn finish_surface_operation(
        &mut self,
        active: &ActiveOperation,
        outcome: &OperationOutcome,
        runtime_usage: UsageTotals,
        completed_turn: Option<CompletedTurnOutcome>,
        goal_finalization: Option<PreparedGoalSurfaceFinalization>,
    ) -> Result<Option<surface::OperationTerminalAtCursor>, RuntimeHostError> {
        let surface_usage = surface_usage_totals(runtime_usage);
        let fence = active.surface_operation.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "typed surface operation fence is missing during finalization".to_string(),
            }
        })?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let durable_remote_cleanup_ambiguous = snapshot.tools.iter().any(|tool| {
            tool.terminal_leases.iter().any(|lease| {
                matches!(
                    lease.state,
                    surface::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous { .. }
                )
            })
        });
        let durable_external_effect_ambiguous = snapshot.tools.iter().any(|tool| {
            tool.capability_calls.iter().any(|call| {
                call.fence == fence
                    && matches!(
                        &call.state,
                        surface::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
                    )
            })
        });
        let execution_failure = durable_remote_cleanup_ambiguous
            .then_some(surface::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous)
            .or_else(|| {
                durable_external_effect_ambiguous
                    .then_some(surface::GenerationExecutionFailureClass::ExternalEffectAmbiguous)
            })
            .or(active.surface_execution_failure);
        let (stop_reason, terminal) = if let Some(class) = execution_failure {
            let (fallback, terminal_class) = match class {
                surface::GenerationExecutionFailureClass::Provider => {
                    ("provider execution failed", surface::FailureClass::Provider)
                }
                surface::GenerationExecutionFailureClass::Tool => {
                    ("tool execution failed", surface::FailureClass::Tool)
                }
                surface::GenerationExecutionFailureClass::Hook => {
                    ("hook execution failed", surface::FailureClass::Hook)
                }
                surface::GenerationExecutionFailureClass::Workflow => {
                    ("workflow execution failed", surface::FailureClass::Workflow)
                }
                surface::GenerationExecutionFailureClass::InputResolution => (
                    "input resolution failed",
                    surface::FailureClass::InputResolution,
                ),
                surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable => (
                    "required client capability became unavailable",
                    surface::FailureClass::ClientCapabilityUnavailable,
                ),
                surface::GenerationExecutionFailureClass::LegacyApprovalRequired => (
                    "legacy approval is required",
                    surface::FailureClass::LegacyApprovalRequired,
                ),
                surface::GenerationExecutionFailureClass::RuntimeInvariant => (
                    "runtime invariant failed",
                    surface::FailureClass::RuntimeInvariant,
                ),
                surface::GenerationExecutionFailureClass::ExternalEffectAmbiguous => (
                    "external file write effect is ambiguous",
                    surface::FailureClass::ExternalEffectAmbiguous,
                ),
                surface::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous => (
                    "remote resource cleanup is ambiguous",
                    surface::FailureClass::RemoteResourceCleanupAmbiguous,
                ),
            };
            let message = active
                .surface_execution_failure_diagnostic
                .clone()
                .unwrap_or_else(|| {
                    surface::SafeDiagnosticText::try_new(fallback)
                        .expect("fixed diagnostic is bounded")
                });
            (
                surface::GenerationStopReason::ExecutionFailed {
                    class,
                    message: message.clone(),
                },
                surface::OperationTerminal::Failed {
                    class: terminal_class,
                    message,
                },
            )
        } else {
            match (outcome, &completed_turn) {
                (
                    OperationOutcome::Completed(RunStatus::Success),
                    Some(CompletedTurnOutcome {
                        status: RunStatus::Success,
                        ..
                    }),
                ) => (
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::Success,
                    },
                    surface::OperationTerminal::Succeeded {
                        usage: surface_usage.clone(),
                    },
                ),
                (
                    OperationOutcome::Completed(RunStatus::Cancelled),
                    Some(CompletedTurnOutcome {
                        status: RunStatus::Cancelled,
                        ..
                    }),
                ) => {
                    let cause = active
                        .surface_terminalization
                        .unwrap_or(surface::TerminalizationCause::UserCancel);
                    let terminal = match cause {
                        surface::TerminalizationCause::HostShutdown => {
                            surface::OperationTerminal::Shutdown {
                                reason: surface::SurfaceShutdownReason::HostShutdown,
                            }
                        }
                        surface::TerminalizationCause::ThreadClose => {
                            surface::OperationTerminal::Shutdown {
                                reason: surface::SurfaceShutdownReason::ThreadClose,
                            }
                        }
                        surface::TerminalizationCause::GoalPause => {
                            surface::OperationTerminal::Cancelled {
                                reason: surface::CancelReason::GoalPause,
                            }
                        }
                        surface::TerminalizationCause::UserCancel => {
                            surface::OperationTerminal::Cancelled {
                                reason: surface::CancelReason::User,
                            }
                        }
                    };
                    (surface::GenerationStopReason::Cancelled { cause }, terminal)
                }
                (OperationOutcome::Stopped(terminal), Some(_completed)) => {
                    // The typed terminal owns the budget facts; the surface
                    // projection derives its budget from the stop reason with
                    // the precise dimension (tool calls and wall time are
                    // never conflated with turn requests or tokens).
                    let budget = match terminal {
                        orca_core::budget::OperationTerminal::Stopped { reason, usage, .. } => {
                            match reason {
                                orca_core::budget::StopReason::TurnBudget { max_turns } => {
                                    surface::OperationBudget::TurnRequests {
                                        scope: surface::TurnRequestBudgetScope::AgentLoop,
                                        limit: u64::from(*max_turns),
                                        observed: u64::from(usage.turns),
                                    }
                                }
                                orca_core::budget::StopReason::ToolCallBudget {
                                    max_tool_calls,
                                } => surface::OperationBudget::ToolCalls {
                                    limit: u64::from(*max_tool_calls),
                                    observed: u64::from(usage.tool_calls),
                                },
                                orca_core::budget::StopReason::CostBudget {
                                    max_cost_usd_micros,
                                } => surface::OperationBudget::MonetaryBudgetUsdMicros {
                                    limit: *max_cost_usd_micros,
                                    observed: usage.cost_usd_micros,
                                },
                                orca_core::budget::StopReason::WallTimeBudget {
                                    max_wall_time_ms,
                                } => surface::OperationBudget::WallTimeMs {
                                    limit: *max_wall_time_ms,
                                    observed: usage.wall_time_ms,
                                },
                            }
                        }
                        _ => surface::OperationBudget::ModelTokens {
                            limit: None,
                            observed: None,
                        },
                    };
                    (
                        surface::GenerationStopReason::Completed {
                            status: surface::GenerationCompletionStatus::BudgetExhausted {
                                budget: budget.clone(),
                            },
                        },
                        surface::OperationTerminal::BudgetExhausted { budget },
                    )
                }
                (
                    OperationOutcome::Completed(RunStatus::ApprovalRequired),
                    Some(CompletedTurnOutcome {
                        status: RunStatus::ApprovalRequired,
                        ..
                    }),
                ) => {
                    let message = surface::SafeDiagnosticText::try_new(
                        "foreground operation requires approval",
                    )
                    .expect("fixed diagnostic is bounded");
                    (
                        surface::GenerationStopReason::ExecutionFailed {
                            class: surface::GenerationExecutionFailureClass::LegacyApprovalRequired,
                            message: message.clone(),
                        },
                        surface::OperationTerminal::Failed {
                            class: surface::FailureClass::LegacyApprovalRequired,
                            message,
                        },
                    )
                }
                (
                    OperationOutcome::Completed(RunStatus::VerificationFailed),
                    Some(CompletedTurnOutcome {
                        status: RunStatus::VerificationFailed,
                        ..
                    }),
                ) => {
                    let message = surface::SafeDiagnosticText::try_new(
                        "foreground operation verification failed",
                    )
                    .expect("fixed diagnostic is bounded");
                    (
                        surface::GenerationStopReason::Completed {
                            status: surface::GenerationCompletionStatus::VerificationFailed {
                                message: message.clone(),
                            },
                        },
                        surface::OperationTerminal::Failed {
                            class: surface::FailureClass::Verification,
                            message,
                        },
                    )
                }
                (OperationOutcome::Panicked { message }, _) => (
                    surface::GenerationStopReason::Panicked {
                        message: surface::SafeDiagnosticText::try_new(message.clone())
                            .unwrap_or_else(|_| {
                                surface::SafeDiagnosticText::try_new("generation panicked").unwrap()
                            }),
                    },
                    surface::OperationTerminal::Panicked {
                        message: surface::SafeDiagnosticText::try_new(message.clone())
                            .unwrap_or_else(|_| {
                                surface::SafeDiagnosticText::try_new("generation panicked").unwrap()
                            }),
                    },
                ),
                _ => {
                    let message =
                        surface::SafeDiagnosticText::try_new("foreground operation failed")
                            .expect("fixed diagnostic is bounded");
                    (
                        surface::GenerationStopReason::ExecutionFailed {
                            class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                            message: message.clone(),
                        },
                        surface::OperationTerminal::Failed {
                            class: surface::FailureClass::RuntimeInvariant,
                            message,
                        },
                    )
                }
            }
        };
        let operation_id = fence.operation_id.clone();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed surface operation is missing during finalization".to_string(),
            })?;
        let original_request_id = operation.request_id.clone();
        let thread_id = snapshot.thread.thread_id.clone();
        let thread_owner_epoch = snapshot.thread.owner_epoch;
        let completion_proof = Self::surface_completion_proof(
            &snapshot,
            operation,
            &terminal,
            completed_turn.as_ref(),
        )?;
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let stream_discard_reason = match &stop_reason {
            surface::GenerationStopReason::Cancelled { .. } => {
                surface::AssistantDiscardReason::GenerationCancelled
            }
            surface::GenerationStopReason::InterruptedResumable => {
                surface::AssistantDiscardReason::GenerationInterrupted
            }
            surface::GenerationStopReason::RuntimeRestart
            | surface::GenerationStopReason::NotStarted {
                reason: surface::NotStartedReason::RuntimeRestart,
            } => surface::AssistantDiscardReason::RuntimeRestart,
            surface::GenerationStopReason::ProjectionFailure { .. } => {
                surface::AssistantDiscardReason::ProjectionRepair
            }
            _ => surface::AssistantDiscardReason::ProviderFailed,
        };
        let generation_scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut stop_and_finalization_events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == fence && stream.state == surface::SurfaceAssistantStreamState::Open
            })
            .map(|stream| {
                (
                    generation_scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: stream_discard_reason,
                    }),
                )
            })
            .collect::<Vec<_>>();
        stop_and_finalization_events.push((
            generation_scope,
            surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                fence: fence.clone(),
                reason: stop_reason.clone(),
                usage_delta: surface::UsageTotals {
                    input_tokens: surface_usage.input_tokens,
                    output_tokens: surface_usage.output_tokens,
                    cache_tokens: surface_usage.cache_tokens,
                    estimated_cost_usd_micros: surface_usage.estimated_cost_usd_micros,
                },
            }),
        ));
        stop_and_finalization_events.push((
            surface::SurfaceScope::Operation {
                operation_id: operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::GenerationStop(stop_reason),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        ));
        if active.request.surface_goal_owned && goal_finalization.is_none() {
            let identity = operation
                .generations
                .last()
                .and_then(|generation| generation.goal_identity.clone())
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "typed Goal finalization lacks its generation identity".to_string(),
                })?;
            let control = self
                .goal_controller
                .active_control(active.operation_id)
                .cloned()
                .ok_or_else(|| RuntimeHostError::GoalControlFailed {
                    message: "typed Goal finalization lacks its runtime owner".to_string(),
                })?;
            let goal_revision = snapshot
                .goal
                .as_ref()
                .ok_or_else(|| RuntimeHostError::GoalControlFailed {
                    message: "typed Goal finalization lacks its snapshot".to_string(),
                })?
                .goal_revision
                .get();
            let active_workflow = self
                .state
                .as_ref()
                .is_some_and(|state| state.thread.session().has_active_workflows());
            let last_model_response = self.state.as_ref().and_then(|state| {
                state
                    .thread
                    .session()
                    .conversation()
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::Assistant { content, .. } => content.clone(),
                        _ => None,
                    })
            });
            self.spawn_goal_surface_finalization(
                GoalSurfaceFinalizationWork {
                    control,
                    identity,
                    goal_revision,
                    terminalization: active.surface_terminalization,
                    terminal: terminal.clone(),
                    surface_usage,
                    completed_turn: completed_turn.clone(),
                    active_workflow,
                    last_model_response,
                    config: active.config.clone(),
                    cancel: active.generation.cancel.clone(),
                    thread_id: snapshot.thread.thread_id.clone(),
                    owner_epoch: snapshot.thread.owner_epoch,
                },
                Vec::new(),
                outcome.clone(),
                runtime_usage,
                completed_turn.clone(),
            )?;
            return Ok(None);
        }
        let goal_settlement = if active.request.surface_goal_owned {
            let settlement =
                goal_finalization.ok_or_else(|| RuntimeHostError::GoalControlFailed {
                    message: "typed Goal finalization lacks its blocking-worker result".to_string(),
                })?;
            let PreparedGoalSurfaceFinalization {
                runtime,
                mutations,
                events,
                finished_fence,
                finished_digest,
                verification_authority,
                decision_fence,
                decision_digest,
            } = settlement;
            stop_and_finalization_events.extend(events);
            Some((
                runtime,
                mutations,
                finished_fence,
                finished_digest,
                verification_authority,
                decision_fence,
                decision_digest,
            ))
        } else {
            if goal_finalization.is_some() {
                return Err(RuntimeHostError::GoalControlFailed {
                    message: "non-Goal finalization received a Goal Store result".to_string(),
                });
            }
            None
        };
        let stop_and_finalization_batch =
            self.surface_event_batch_with_commit_id(stop_and_finalization_events, None);
        let stop_commit = if let Some((
            _,
            _,
            finished_fence,
            finished_digest,
            verification_authority,
            decision_fence,
            decision_digest,
        )) = goal_settlement.as_ref()
        {
            self.resident_surface
                .coordinator
                .commit_goal_generation_stop_batch(
                    finished_fence.clone(),
                    finished_digest.clone(),
                    verification_authority.clone(),
                    decision_fence.clone(),
                    decision_digest.clone(),
                    fence,
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                    &stop_and_finalization_batch,
                )
        } else {
            self.resident_surface
                .coordinator
                .commit_live_generation_stop_disposition_batch(
                    fence,
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                    &stop_and_finalization_batch,
                )
        };
        stop_commit.map_err(|error| RuntimeHostError::ThreadStartFailed {
            message: format!(
                "typed surface generation stop and finalization start failed: {error:?}"
            ),
        })?;
        if let Some((runtime, mutations, ..)) = goal_settlement {
            for mutation in mutations {
                Self::schedule_goal_surface_acknowledgement(runtime.clone(), mutation);
            }
        }
        let usage = surface_usage;
        let terminal_record = surface::OperationTerminalRecord {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal: terminal.clone(),
            usage,
            source_diagnostic_digest: None,
            settlement_receipts: Vec::new(),
            completion_proof,
            committed_at: surface::UnixMillis::new(0),
        };
        let task_terminal_projection = if matches!(
            active.request.operation_kind(),
            HostedOperationKind::BackgroundContinuation { .. }
        ) {
            prepare_main_session_task_terminal_projection(
                self.resident_surface.coordinator.state().snapshot(),
                &operation_id,
                &terminal_record,
            )?
        } else {
            None
        };
        let terminal_batch = if let Some(projection) = task_terminal_projection.as_ref() {
            self.surface_event_batch_with_commit_id(
                vec![
                    projection.event.clone(),
                    (
                        surface::SurfaceScope::Operation {
                            operation_id: operation_id.clone(),
                        },
                        surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal {
                            record: terminal_record.clone(),
                        }),
                    ),
                ],
                Some(terminal_commit_id.clone()),
            )
        } else {
            self.surface_operation_batch_with_commit_id(
                &operation_id,
                vec![surface::OperationPatch::Terminal {
                    record: terminal_record.clone(),
                }],
                Some(terminal_commit_id.clone()),
            )
        };
        let terminal_result = if task_terminal_projection.is_some() {
            self.resident_surface
                .coordinator
                .commit_actor_finalizer_task_terminal_batch(
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                    &terminal_batch,
                )
        } else {
            self.resident_surface.coordinator.commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                &terminal_batch,
            )
        };
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            completion_proof: terminal_record.completion_proof.clone(),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = terminal_result {
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                thread_id,
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            let failure = surface::WaitOperationTerminalResult::TerminalCommitFailure {
                operation_id: operation_id.clone(),
                finalize_intent_id,
                commit_id: terminal_commit_id,
                repair,
            };
            self.cache_surface_terminal_failure(PendingSurfaceTerminalCommit {
                batch: terminal_batch,
                value,
                failure,
                legacy_completion: Some(active.completion.clone()),
                legacy_terminal: Some(OperationTerminal {
                    operation_id: active.operation_id,
                    outcome: outcome.clone(),
                }),
            });
            return Err(RuntimeHostError::ThreadStartFailed {
                message: format!("typed surface terminal commit failed: {error:?}"),
            });
        }
        if let Some(projection) = task_terminal_projection.as_ref() {
            mirror_main_session_task_terminal_projection(
                &self.handle.task_registry,
                &terminal_record,
                projection,
            );
        }
        self.cache_surface_terminal(value.clone());
        Ok(Some(value))
    }

    fn surface_completion_proof(
        snapshot: &surface::SurfaceSnapshot,
        operation: &surface::OperationRecord,
        terminal: &surface::OperationTerminal,
        completed_turn: Option<&CompletedTurnOutcome>,
    ) -> Result<surface::SurfaceOperationCompletionProof, RuntimeHostError> {
        let verification = completed_turn
            .and_then(|completed| completed.verification.as_ref())
            .map(surface::SurfaceVerificationResult::try_from_verification)
            .transpose()
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("invalid verifier completion proof: {error}"),
            })?;
        let mut proof = verification
            .map(surface::SurfaceOperationCompletionProof::from_verification)
            .unwrap_or_default();
        for tool in snapshot.tools.iter().filter(|tool| {
            operation
                .generations
                .iter()
                .any(|generation| generation.logical_turn_id == tool.request.turn_id)
        }) {
            let Some(result) = tool.result.as_ref() else {
                Self::push_completion_limitation(
                    &mut proof,
                    "a tool invocation had no terminal result",
                );
                continue;
            };
            proof
                .tool_receipts
                .push(surface::SurfaceToolCompletionReceipt::from_result(result));
            if matches!(
                result.terminal.kind,
                surface::SurfaceToolResultKind::ExternalEffectAmbiguous
                    | surface::SurfaceToolResultKind::ObservationUnavailable
                    | surface::SurfaceToolResultKind::CleanupAmbiguous
            ) || result.terminal.invocation_started == surface::ToolInvocationStarted::Unknown
            {
                Self::push_completion_limitation(
                    &mut proof,
                    "a tool invocation has an indeterminate outcome",
                );
            }
        }
        if let Some(orca_core::budget::OperationTerminal::Stopped {
            checkpoint_id,
            resumable: true,
            ..
        }) = completed_turn.and_then(|completed| completed.terminal.as_ref())
            && !checkpoint_id.trim().is_empty()
        {
            proof.resume = Some(
                surface::SurfaceResumeBoundary::new(checkpoint_id.clone()).map_err(|error| {
                    RuntimeHostError::ThreadStartFailed {
                        message: format!("invalid completion resume boundary: {error}"),
                    }
                })?,
            );
        }
        if matches!(
            proof.verification,
            surface::SurfaceCompletionVerification::Unverified
        ) {
            Self::push_completion_limitation(&mut proof, "verification was not run");
        }
        match terminal {
            surface::OperationTerminal::Cancelled { .. }
            | surface::OperationTerminal::Shutdown { .. } => {
                Self::push_completion_limitation(&mut proof, "operation was cancelled");
            }
            surface::OperationTerminal::BudgetExhausted { .. } => {
                Self::push_completion_limitation(
                    &mut proof,
                    "operation stopped at a budget boundary",
                );
            }
            surface::OperationTerminal::NotAdmitted { .. } => {
                Self::push_completion_limitation(&mut proof, "operation was not admitted");
            }
            surface::OperationTerminal::Failed { .. }
            | surface::OperationTerminal::Panicked { .. }
            | surface::OperationTerminal::JoinFailed { .. }
            | surface::OperationTerminal::AbortedByRuntimeRestart { .. } => {
                Self::push_completion_limitation(
                    &mut proof,
                    "operation did not complete successfully",
                );
            }
            surface::OperationTerminal::Succeeded { .. } => {}
        }
        proof
            .validate()
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("invalid completion proof: {error}"),
            })?;
        Ok(proof)
    }

    fn push_completion_limitation(
        proof: &mut surface::SurfaceOperationCompletionProof,
        limitation: &'static str,
    ) {
        if proof.limitations.len() < surface::SURFACE_COMPLETION_PROOF_LIMITATION_LIMIT
            && !proof
                .limitations
                .iter()
                .any(|existing| existing.as_str() == limitation)
        {
            proof
                .limitations
                .push(surface::DisplayText::new(limitation));
        }
    }

    pub(super) fn prepare_goal_surface_continuation_work(
        &self,
        active: &ActiveOperation,
        state: &ThreadActorState,
        runtime_usage: UsageTotals,
        completed_turn: CompletedTurnOutcome,
    ) -> Result<Option<GoalSurfaceContinuationWork>, RuntimeHostError> {
        if !active.request.surface_goal_owned
            || active.surface_terminalization.is_some()
            || active.surface_execution_failure.is_some()
        {
            return Ok(None);
        }
        let control = self
            .goal_controller
            .active_control(active.operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::GoalControlFailed {
                message: "typed Goal continuation lacks its runtime owner".to_string(),
            })?;
        let operation_id = active
            .surface_operation
            .as_ref()
            .map(|fence| fence.operation_id.clone())
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed Goal continuation lost its foreground operation".to_string(),
            })?;
        let conversation = state.thread.session().conversation();
        Ok(Some(GoalSurfaceContinuationWork {
            control,
            snapshot: self.resident_surface.coordinator.state().snapshot().clone(),
            operation_id,
            surface_usage: surface_usage_totals(runtime_usage),
            completed_turn,
            active_workflow: state.thread.session().has_active_workflows(),
            last_model_response: conversation.messages.iter().rev().find_map(
                |message| match message {
                    Message::Assistant { content, .. } => content.clone(),
                    _ => None,
                },
            ),
            plan_snapshot: conversation
                .internal_context
                .get(orca_core::conversation::PLAN_CONTEXT_FRAGMENT_ID)
                .map(|fragment| fragment.content.clone()),
            previous_checkpoint: previous_assistant_checkpoint(conversation),
            config: active.config.clone(),
            cancel: active.generation.cancel.clone(),
        }))
    }

    pub(super) fn settle_prepared_goal_surface_continuation(
        &mut self,
        prepared: PreparedGoalSurfaceContinuation,
    ) -> Result<(surface::SurfaceGoalGenerationIdentity, String, String), RuntimeHostError> {
        let PreparedGoalSurfaceContinuation {
            runtime,
            mutations,
            continuation_events,
            finished_fence,
            finished_digest,
            verification_authority,
            decision_fence,
            decision_digest,
            predecessor_fence,
            successor,
            continuation_prompt,
            legacy_task_id,
            started_commit_id,
            started_events,
            resolved_events,
            loop_events,
        } = prepared;
        let continuation_batch = self.surface_event_batch_with_commit_id(continuation_events, None);
        self.resident_surface
            .coordinator
            .commit_goal_generation_continue_batch(
                finished_fence,
                finished_digest,
                verification_authority,
                decision_fence,
                decision_digest,
                predecessor_fence,
                &continuation_batch,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("typed Goal continuation commit failed: {error:?}"),
            })?;
        for mutation in mutations {
            Self::schedule_goal_surface_acknowledgement(runtime.clone(), mutation);
        }
        let successor_fence = successor.operation_fence.clone();
        let started_batch = self.surface_operation_batch_with_commit_id(
            &successor_fence.operation_id,
            started_events,
            Some(started_commit_id),
        );
        self.resident_surface
            .coordinator
            .commit_generation_batch(successor_fence.clone(), &started_batch)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("typed Goal continuation start failed: {error:?}"),
            })?;
        let resolved_batch = self.surface_event_batch_with_commit_id(resolved_events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(successor_fence.clone(), &resolved_batch)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("typed Goal continuation input resolution failed: {error:?}"),
            })?;
        let loop_batch = self.surface_operation_batch(&successor_fence.operation_id, loop_events);
        self.resident_surface
            .coordinator
            .commit_generation_batch(successor_fence, &loop_batch)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("typed Goal continuation loop start failed: {error:?}"),
            })?;
        Ok((successor, continuation_prompt, legacy_task_id))
    }

    pub(super) fn fail_surface_goal_continuation(
        &mut self,
        active: ActiveOperation,
        mut result: OperationTaskResult,
        message: String,
    ) {
        let _ = result.writer.finish_generation(true);
        self.state = Some(result.state);
        self.dispatch_surface_goal_completion_recovery(active, message);
    }

    pub(super) fn start_surface_goal_continuation(
        &mut self,
        mut active: ActiveOperation,
        mut result: OperationTaskResult,
        successor: surface::SurfaceGoalGenerationIdentity,
        continuation_prompt: String,
        legacy_task_id: String,
    ) -> Result<(), RuntimeHostError> {
        if let Err(error) = result.writer.finish_generation(false) {
            self.fail_surface_goal_continuation(
                active,
                result,
                format!("typed Goal continuation writer failed after durable admission: {error}"),
            );
            return Ok(());
        }
        if let Some(task_id) = active.runtime_task_id.as_deref() {
            result
                .state
                .thread
                .lifecycle_mut()
                .start_task_with_id(RuntimeTaskKind::Agent, task_id);
        }
        let goal_id = orca_core::goal_runtime::GoalId::parse(successor.goal_id.as_str())
            .map_err(|message| RuntimeHostError::GoalControlFailed { message })?;
        let goal_run_id =
            orca_core::goal_runtime::GoalRunId::parse(successor.goal_run_id.as_str().to_string())
                .map_err(|message| RuntimeHostError::GoalControlFailed { message })?;
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::parse(
            successor.goal_outer_turn_id.as_str().to_string(),
        )
        .map_err(|message| RuntimeHostError::GoalControlFailed { message })?;
        active.request.prompt = continuation_prompt;
        active.request.turn_id = successor.logical_turn_id.clone();
        active.request.task_id = Some(legacy_task_id);
        active.request.continuation = None;
        active.request.goal_turn_origin = orca_core::goal_runtime::GoalTurnOrigin::Continuation;
        active.request.resumes_existing_turn = false;
        let successor_turn = crate::goal_actor::GoalTurnContext {
            session_id: self
                .goal_controller
                .active_control(active.operation_id)
                .cloned()
                .expect("surface Goal continuation keeps control")
                .session_id,
            goal_id,
            goal_run_id,
            outer_turn_id,
            origin: orca_core::goal_runtime::GoalTurnOrigin::Continuation,
            run_started: false,
        };
        assert!(
            self.goal_controller
                .replace_active_turn(active.operation_id, successor_turn),
            "surface Goal continuation keeps turn ownership",
        );
        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = successor.operation_fence.clone();
        active.request.generation_handler_factory = Some(Arc::new(move |_, cancel| {
            HostedGenerationHandlers::default()
                .with_provider_response_ingress(Arc::new(RuntimeSurfaceProviderResponseIngress {
                    command_tx: interaction_command_tx.clone(),
                    fence: interaction_fence.clone(),
                }))
                .with_workflow_lifecycle_ingress(Arc::new(RuntimeSurfaceWorkflowLifecycleIngress {
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
        }));
        let context = GenerationContext::new(
            active.generation.context.fence().next(),
            active.steer_handle.clone(),
            false,
            HostedGenerationHandlers::default(),
            active.config.clone(),
        );
        active.surface_operation = Some(successor.operation_fence);
        active.generation = self.spawn_generation(
            result.state,
            &active.request,
            self.goal_controller
                .active_turn(active.operation_id)
                .cloned(),
            result.writer,
            context,
        );
        self.active = Some(active);
        Ok(())
    }

    pub(super) fn finish_surface_goal_without_continuation(
        &mut self,
        active: ActiveOperation,
        mut result: OperationTaskResult,
        runtime_usage: UsageTotals,
    ) -> Result<(), RuntimeHostError> {
        let writer_error = result.writer.finish_generation(true).err();
        let (outcome, completed_turn) = if let Some(error) = writer_error {
            self.state = Some(result.state);
            (
                OperationOutcome::ExecutionFailed {
                    kind: error.kind(),
                    message: error.to_string(),
                },
                None,
            )
        } else {
            match result.outcome {
                GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
                    status,
                    end_reason,
                    terminal,
                    verification,
                    ..
                }) => {
                    self.state = Some(result.state);
                    let outcome = match &terminal {
                        Some(orca_core::budget::OperationTerminal::Stopped { .. }) => {
                            OperationOutcome::Stopped(terminal.clone().expect("matched stop"))
                        }
                        _ => OperationOutcome::Completed(status),
                    };
                    (
                        outcome,
                        Some(CompletedTurnOutcome {
                            status,
                            end_reason,
                            terminal,
                            verification,
                        }),
                    )
                }
                GenerationTaskOutcome::Executed(ThreadOperationOutcome::ProviderSuspended {
                    ..
                }) => {
                    self.state = Some(result.state);
                    (
                        OperationOutcome::ExecutionFailed {
                            kind: io::ErrorKind::Other,
                            message: "typed Goal continuation unexpectedly suspended its provider"
                                .to_string(),
                        },
                        None,
                    )
                }
                GenerationTaskOutcome::ExecutionFailed { kind, message } => {
                    self.state = Some(result.state);
                    (OperationOutcome::ExecutionFailed { kind, message }, None)
                }
                GenerationTaskOutcome::Panicked { message } => {
                    self.state = Some(result.state);
                    (OperationOutcome::Panicked { message }, None)
                }
            }
        };
        match self.finish_surface_operation(&active, &outcome, runtime_usage, completed_turn, None)
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.active = Some(active);
                return Ok(());
            }
            Err(error) => {
                let operation_id = active
                    .surface_operation
                    .as_ref()
                    .map(|fence| fence.operation_id.clone())
                    .expect("surface Goal continuation keeps its operation");
                if self
                    .resident_surface
                    .commit
                    .has_pending_terminal(&operation_id)
                {
                    return Ok(());
                }
                self.dispatch_surface_goal_completion_recovery(active, error.to_string());
                return Ok(());
            }
        }
        if let Some(fence) = active.surface_operation.as_ref() {
            self.generation_context_controller
                .clear_operation(&fence.operation_id);
        }
        self.goal_controller.clear_active(active.operation_id);
        let completed = active.completion.complete(OperationTerminal {
            operation_id: active.operation_id,
            outcome,
        });
        debug_assert!(completed, "operation terminal must complete exactly once");
        Ok(())
    }

    pub(super) fn dispatch_surface_goal_continuation(
        &mut self,
        active: ActiveOperation,
        result: OperationTaskResult,
        runtime_usage: UsageTotals,
        completed_turn: CompletedTurnOutcome,
    ) -> Result<(), RuntimeHostError> {
        let Some(work) = self.prepare_goal_surface_continuation_work(
            &active,
            &result.state,
            runtime_usage,
            completed_turn,
        )?
        else {
            return self.finish_surface_goal_without_continuation(active, result, runtime_usage);
        };
        if self.goal_controller.is_blocking() {
            self.fail_surface_goal_continuation(
                active,
                result,
                "another Goal Store request is still in flight".to_string(),
            );
            return Ok(());
        }
        let spawned = self.spawn_goal_blocking(
            "typed Goal continuation preview and commit",
            GoalBlockingCompletionKind::PreviewCommit,
            move || prepare_goal_surface_continuation_worker(work),
            move |actor, worker| match worker {
                Ok(GoalSurfaceContinuationWorkerResult::Reconcile { work, worker }) => {
                    let acknowledgements = worker.mutations.clone();
                    let reconciliation = worker.mutations.iter().try_for_each(|mutation| {
                        actor
                            .commit_goal_surface_mutation_with_retry(mutation)
                            .map(|_| ())
                    });
                    if let Err(error) = reconciliation {
                        actor.fail_surface_goal_continuation(active, result, error.to_string());
                        return;
                    }
                    if actor.goal_controller.is_blocking() {
                        actor.fail_surface_goal_continuation(
                            active,
                            result,
                            "another Goal Store request is still in flight".to_string(),
                        );
                        return;
                    }
                    let spawned = actor.spawn_goal_blocking(
                        "typed Goal continuation preview and commit",
                        GoalBlockingCompletionKind::PreviewCommit,
                        move || {
                            for mutation in acknowledgements {
                                let acknowledged = work
                                    .control
                                    .runtime
                                    .acknowledge_surface_mutation(
                                        &mutation.receipt.store_commit_id,
                                        &mutation.receipt.receipt_digest,
                                    )
                                    .map_err(|error| RuntimeHostError::GoalControlFailed {
                                        message: error.to_string(),
                                    })?;
                                if !acknowledged {
                                    return Err(RuntimeHostError::GoalControlFailed {
                                        message: "typed Goal continuation reconciliation rejected its exact receipt"
                                            .to_string(),
                                    });
                                }
                            }
                            prepare_goal_surface_continuation_worker(work)
                        },
                        move |actor, worker| {
                            actor.finish_surface_goal_continuation_worker(
                                active,
                                result,
                                runtime_usage,
                                worker,
                            );
                        },
                    );
                    debug_assert!(spawned.is_ok(), "Goal continuation retry was prevalidated");
                }
                worker => actor.finish_surface_goal_continuation_worker(
                    active,
                    result,
                    runtime_usage,
                    worker,
                ),
            },
        );
        debug_assert!(spawned.is_ok(), "Goal continuation worker was prevalidated");
        Ok(())
    }

    pub(super) fn finish_surface_goal_continuation_worker(
        &mut self,
        active: ActiveOperation,
        result: OperationTaskResult,
        runtime_usage: UsageTotals,
        worker: Result<GoalSurfaceContinuationWorkerResult, RuntimeHostError>,
    ) {
        match worker {
            Ok(GoalSurfaceContinuationWorkerResult::Prepared(prepared)) => {
                match self.settle_prepared_goal_surface_continuation(prepared) {
                    Ok((successor, prompt, task_id)) => {
                        if let Err(error) = self.start_surface_goal_continuation(
                            active, result, successor, prompt, task_id,
                        ) {
                            self.operation_recovery.terminal_blocked = Some(error.to_string());
                        }
                    }
                    Err(error) => {
                        self.fail_surface_goal_continuation(active, result, error.to_string());
                    }
                }
            }
            Ok(GoalSurfaceContinuationWorkerResult::NoContinuation) => {
                if let Err(error) =
                    self.finish_surface_goal_without_continuation(active, result, runtime_usage)
                {
                    self.operation_recovery.terminal_blocked = Some(error.to_string());
                }
            }
            Ok(GoalSurfaceContinuationWorkerResult::Reconcile { .. }) => {
                self.fail_surface_goal_continuation(
                    active,
                    result,
                    "typed Goal continuation reconciliation did not converge".to_string(),
                );
            }
            Err(error) => {
                self.fail_surface_goal_continuation(active, result, error.to_string());
            }
        }
    }

    pub(super) fn prepare_manual_compaction_completed_batch(
        &self,
        active: &ActiveOperation,
        after_messages: u64,
        compaction: Option<&crate::session::ManualCompactionOutcome>,
    ) -> Result<surface::SurfaceCommitBatch, RuntimeHostError> {
        let Some(before_messages) = active.surface_manual_compaction_before_messages else {
            return Err(RuntimeHostError::ThreadStartFailed {
                message: "typed manual compaction completion metadata is missing".to_string(),
            });
        };
        let fence = active.surface_operation.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "typed manual compaction operation fence is missing".to_string(),
            }
        })?;
        let events = self
            .generation_context_controller
            .manual_compaction_completed_events(
                self.resident_surface.coordinator.state().snapshot(),
                &fence,
                before_messages,
                after_messages,
                compaction,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: error.to_string(),
            })?;
        Ok(self.surface_event_batch_with_commit_id(events, None))
    }

    pub(super) fn prepare_manual_compaction_idle_batch(
        &self,
        active: &ActiveOperation,
    ) -> Result<surface::SurfaceCommitBatch, RuntimeHostError> {
        let fence = active.surface_operation.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "typed manual compaction operation fence is missing".to_string(),
            }
        })?;
        let events = self
            .generation_context_controller
            .manual_compaction_idle_events(
                self.resident_surface.coordinator.state().snapshot(),
                &fence,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: error.to_string(),
            })?;
        Ok(self.surface_event_batch_with_commit_id(events, None))
    }

    pub(super) fn bind_surface_operation_controller(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        operation_id: &surface::SurfaceOperationId,
    ) -> bool {
        let existing = self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(operation_id)
            .cloned();
        if existing.as_ref() == Some(client.attachment_id()) {
            return true;
        }
        if existing
            .as_ref()
            .is_some_and(|bound| self.resident_surface.hub.has_live_attachment(bound))
        {
            return false;
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let visible = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .any(|operation| &operation.operation_id == operation_id)
            || snapshot
                .background_operations
                .iter()
                .any(|operation| &operation.operation_id == operation_id);
        if visible {
            self.resident_surface
                .interactions
                .operation_origin_attachments
                .insert(operation_id.clone(), client.attachment_id().clone());
        }
        visible
    }

    pub(super) fn resume_surface_operation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        expected_last_generation: surface::SurfaceGenerationId,
        resume_source: surface::ResumeSourceWitness,
        restored_goal_binding: Option<RestoredGoalSurfaceBinding>,
    ) -> Result<SurfaceResumeAttempt, surface::SurfaceClientCommandError> {
        if self.operation_recovery.pending_manual_compaction.is_some()
            || !self.bind_surface_operation_controller(client, &operation_id)
            || !self.resident_surface.commit.pending_terminals_empty()
            || self.resident_surface.commit.has_pending_admission()
            || self.operation_recovery.terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if !matches!(
            operation.phase,
            surface::OperationPhase::Suspended {
                cause: surface::SuspensionCause::Interrupted { .. }
                    | surface::SuspensionCause::RecoveryRequired { .. }
                    | surface::SuspensionCause::ProviderSuspended { .. }
            }
        ) || operation.pending_control.is_some()
            || operation.finalization.is_some()
            || operation.terminal.is_some()
            || !matches!(
                operation.intent.kind,
                surface::OperationKind::UserTurn | surface::OperationKind::GoalRun { .. }
            )
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let previous = operation
            .generations
            .last()
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if previous.fence.generation_id != expected_last_generation
            || previous.phase != surface::GenerationPhase::Stopped
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let (input_request, request_digest) = match (&previous.replayability, &resume_source) {
            (
                surface::Replayability::Replayable {
                    request: Some(input),
                    request_digest: Some(request_digest),
                    ..
                },
                surface::ResumeSourceWitness::DurableReplay {
                    replayability_digest,
                },
            ) if replayability_digest
                == &surface::canonical_replayability_digest(&previous.replayability) =>
            {
                (input.clone(), request_digest.clone())
            }
            (
                surface::Replayability::NonReplayable {
                    live_capsule: surface::LiveOperationCapsule::Available { incarnation },
                    ..
                },
                surface::ResumeSourceWitness::LiveCapsule {
                    incarnation: witness,
                },
            ) if incarnation == witness && witness == &snapshot.cursor.incarnation => {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let resolved_input = resolve_surface_input(&input_request)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let generation_id = surface::SurfaceGenerationId::new(
            previous
                .fence
                .generation_id
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        );
        let fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id,
        };
        let resume_turn_id = previous.logical_turn_id.clone();
        let goal_identity = previous.goal_identity.as_ref().map(|identity| {
            let mut replacement = identity.clone();
            replacement.operation_fence = fence.clone();
            replacement.logical_turn_id = resume_turn_id.clone();
            replacement.attempt = surface::GenerationAttempt::RecoveryReplacement;
            replacement.predecessor_fence = Some(previous.fence.clone());
            replacement
        });
        let generation = surface::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: resume_turn_id.clone(),
            input: previous.input.clone(),
            predecessor: Some(previous.fence.clone()),
            attempt: surface::GenerationAttempt::RecoveryReplacement,
            goal_identity,
            replayability: previous.replayability.clone(),
            required_capabilities: previous.required_capabilities.clone(),
            capability_fingerprint: previous.capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let restored_goal_binding = match (generation.goal_identity.as_ref(), restored_goal_binding)
        {
            (Some(goal_identity), None) => {
                let session_id = self
                    .handle
                    .session_id
                    .clone()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let runtime = self
                    .state
                    .as_ref()
                    .and_then(|state| state.thread.initialized_goal_runtime_handle())
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                return Ok(SurfaceResumeAttempt::GoalRestoreRequired {
                    runtime,
                    session_id,
                    identity: goal_identity.clone(),
                });
            }
            (Some(goal_identity), Some(restored))
                if restored.identity == *goal_identity
                    && self.handle.session_id.as_deref() == Some(restored.session_id.as_str()) =>
            {
                Some(restored)
            }
            (None, None) => None,
            _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        };
        let resume_batch = self.surface_operation_batch(
            &operation_id,
            vec![
                surface::OperationPatch::GenerationReserved {
                    generation: generation.clone(),
                },
                surface::OperationPatch::ControlIntentCommitted {
                    operation_id: operation_id.clone(),
                    request_id: operation.request_id.clone(),
                    intent: surface::PendingControlIntent::ResumeStarting {
                        generation_fence: fence.clone(),
                    },
                },
            ],
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_actor_batch(&resume_batch)
        {
            eprintln!("orca: typed surface resume reservation commit failed: {error:?}");
            let _ = self
                .repair_surface_resume_failure(&fence, "typed surface resume reservation failed");
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let started_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: fence.clone(),
                witness: surface::GenerationStartedWitness {
                    started_commit_id: started_commit_id.clone(),
                    settings_revision: operation.intent.settings_revision,
                    policy_epoch: operation.intent.policy_epoch,
                    durable_replayability_digest: surface::canonical_replayability_digest(
                        &previous.replayability,
                    ),
                    capability_fingerprint: previous.capability_fingerprint.clone(),
                },
            }],
            Some(started_commit_id),
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &started_batch)
        {
            eprintln!("orca: typed surface resume Started commit failed: {error:?}");
            if self
                .repair_surface_resume_failure(&fence, "typed surface resume Started commit failed")
                .is_err()
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface resume repair failed for {:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        if let surface::GenerationInputState::Pending { input_item_id, .. } = &generation.input {
            let resolved_fact = surface::SurfaceResolvedInputFact::Replayable {
                input: surface_input_for_persisted_presentation(&resolved_input),
                request_digest: request_digest.clone(),
            };
            let resolved_batch = self.surface_event_batch_with_commit_id(
                vec![
                    (
                        surface::SurfaceScope::Generation {
                            fence: fence.clone(),
                        },
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::InputBindingsResolved {
                                fence: fence.clone(),
                                input_item_id: input_item_id.clone(),
                                fact: resolved_fact.clone(),
                            },
                        ),
                    ),
                    (
                        surface::SurfaceScope::Generation {
                            fence: fence.clone(),
                        },
                        surface::SurfaceEvent::Item(surface::ItemPatch::InputResolved {
                            item_id: input_item_id.clone(),
                            fact: resolved_fact,
                        }),
                    ),
                ],
                None,
            );
            if let Err(error) = self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence.clone(), &resolved_batch)
            {
                eprintln!("orca: typed surface resume input commit failed: {error:?}");
                if self
                    .repair_surface_resume_failure(
                        &fence,
                        "typed surface resume input resolution failed",
                    )
                    .is_err()
                {
                    self.operation_recovery.terminal_blocked = Some(format!(
                        "typed surface resume repair failed for {:?}",
                        fence.operation_id
                    ));
                }
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        } else if !matches!(
            generation.input,
            surface::GenerationInputState::Resolved { .. }
        ) {
            let _ = self.repair_surface_resume_failure(
                &fence,
                "typed surface resume input state was invalid",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let legacy_task_id = format!("typed-resume-{}", uuid::Uuid::now_v7());
        let loop_started_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn {
                    turn_id: resume_turn_id.clone(),
                    fence: fence.clone(),
                    ordinal: 0,
                    task_id: surface::SurfaceTaskId::try_new(legacy_task_id.clone())
                        .expect("generated task id is non-empty"),
                    task_status: surface::SurfaceTaskRunningStatus::Running,
                },
            }],
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &loop_started_batch)
        {
            eprintln!("orca: typed surface resume loop commit failed: {error:?}");
            if self
                .repair_surface_resume_failure(&fence, "typed surface resume loop failed")
                .is_err()
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface resume repair failed for {:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        debug_assert_eq!(
            request_digest,
            match &previous.replayability {
                surface::Replayability::Replayable {
                    request_digest: Some(request_digest),
                    ..
                } => request_digest.clone(),
                _ => unreachable!("resume replayability was checked"),
            }
        );
        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = fence.clone();
        let mut hosted_request = HostedTurnRequest::new(resolved_input.canonical_text.as_str())
            .with_backtrack_target(matches!(
                &operation.intent.origin,
                surface::OperationOrigin::TuiUser
            ))
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
                    .with_acp_read_text_file_handler(Arc::new(RuntimeSurfaceReadTextFileHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_acp_write_text_file_handler(Arc::new(
                        RuntimeSurfaceWriteTextFileHandler {
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
                    .with_mcp_elicitation_handler(Arc::new(RuntimeSurfaceMcpElicitationHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                        cancel,
                    }))
            });
        if let Some(restored) = restored_goal_binding.as_ref() {
            hosted_request = hosted_request
                .with_operation_kind(HostedOperationKind::GoalRun)
                .with_goal_tools(true)
                .with_goal_usage_tracking(true)
                .with_goal_turn_origin(restored.origin)
                .with_surface_goal_owned(restored.turn.clone());
        }
        hosted_request.turn_id = resume_turn_id;
        hosted_request.task_id = Some(legacy_task_id);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(hosted_request),
            writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
            config: None,
            reply: start_tx,
        });
        let start_result = match start_rx.recv() {
            Ok(result) => result,
            Err(_) => {
                let _ = self.repair_surface_resume_failure(
                    &fence,
                    "typed surface resume start reply channel closed",
                );
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        };
        if start_result.is_err() {
            if self
                .repair_surface_resume_failure(&fence, "typed surface resume start failed")
                .is_err()
            {
                self.operation_recovery.terminal_blocked = Some(format!(
                    "typed surface resume repair failed for {:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let Some(active) = self.active.as_mut() else {
            let _ = self.repair_surface_resume_failure(
                &fence,
                "typed surface resume active generation was missing",
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        active.surface_operation = Some(fence.clone());
        Ok(SurfaceResumeAttempt::Completed(
            Self::committed_surface_resume_mutation(
                request_id,
                operation_id,
                fence,
                &resume_batch,
                &started_batch,
            ),
        ))
    }

    pub(super) fn cancel_surface_before_admission(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let operation = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let (value, terminal_batch) = self.terminalize_surface_reservation(
            operation_id.clone(),
            surface::ReservationFinalizerReason::CancelledBeforeAdmission,
            surface::NotAdmittedReason::CancelledBeforeAdmission,
        )?;
        debug_assert_eq!(operation.operation_id, operation_id);
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id,
            &terminal_batch,
            surface::CancelOperationOutput::CancelledBeforeAdmission { terminal: value },
        ))
    }

    pub(super) fn cancel_surface_idle(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self.operation_recovery.pending_manual_compaction.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !self.bind_surface_operation_controller(client, &operation_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        if snapshot
            .queued_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
        {
            return self.cancel_surface_before_admission(client, request_id, operation_id);
        }
        if let Some(terminal) = self
            .resident_surface
            .commit
            .terminal(&operation_id)
            .cloned()
        {
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: terminal.cursor.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::OperationTerminalAck {
                            thread_id: terminal.cursor.thread_id.clone(),
                            thread_owner_epoch: snapshot.thread.owner_epoch,
                            operation_id: operation_id.clone(),
                            value: terminal.clone(),
                        },
                    ])
                    .expect("terminal replay has one acknowledgement"),
                },
                value: surface::CancelOperationOutput::AlreadyTerminal { terminal },
            });
        }
        if snapshot
            .background_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
        {
            if self.background_controller.has_provider_matching(|typed| {
                typed.fence.operation_fence.operation_id == operation_id
            }) {
                return self.cancel_surface_background_provider(
                    request_id,
                    operation_id,
                    &snapshot,
                );
            }
            return self.cancel_surface_background_workflow(request_id, operation_id, &snapshot);
        }
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if let Some(finalization) = operation.finalization.as_ref() {
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: snapshot.thread.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::ThreadLocalCursor {
                            cursor: finalization.started_at.cursor.clone(),
                            family: surface::SurfaceFactFamily::Operation,
                            event_id: finalization.started_at.event_id.clone(),
                            commit_class: finalization.started_at.commit_class.clone(),
                        },
                    ])
                    .expect("finalization replay has one acknowledgement"),
                },
                value: surface::CancelOperationOutput::FinalizationPending {
                    operation_id,
                    finalize_intent_id: finalization.finalize_intent_id.clone(),
                    finalization_cursor: finalization.started_at.clone(),
                    waiter: surface::OperationWaiterHandle::new(),
                },
            });
        }
        let surface::OperationPhase::Suspended { .. } = operation.phase else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let suspended_cause = surface::SuspendedFinalizationCause::Terminalization(
            surface::TerminalizationCause::UserCancel,
        );
        let control_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::ControlIntentCommitted {
                operation_id: operation_id.clone(),
                request_id: operation.request_id.clone(),
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: operation_id.clone(),
                    cause: surface::TerminalizationCause::UserCancel,
                },
            }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&control_batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let finalization_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::Suspended(
                    suspended_cause.clone(),
                ),
                suspended_cause: Some(suspended_cause),
                expected_settlements: Vec::new(),
            }],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                &finalization_batch,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let terminal = surface::OperationTerminal::Cancelled {
            reason: surface::CancelReason::User,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
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
                        "cancelled terminal has no verifier proof",
                    ),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id),
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(operation_id.clone(), finalize_intent_id, &terminal_batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let terminal_at_cursor = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                "cancelled terminal has no verifier proof",
            ),
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        self.cache_surface_terminal(terminal_at_cursor);
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &control_batch,
            surface::CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor: control_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    pub(super) fn resolve_surface_acp_prompt_operation(
        &self,
        client: &surface::RuntimeSurfaceClientHandle,
        session_id: &surface::NonEmptyText,
        inbound_seq: surface::SequenceNumber,
    ) -> Result<Option<surface::SurfaceOperationId>, surface::SurfaceClientCommandError> {
        let connection_id = client
            .connection_id()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let mut matches = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .filter(|operation| {
                matches!(
                    &operation.intent.origin,
                    surface::OperationOrigin::AcpPrompt {
                        connection_id: origin_connection_id,
                        session_id: origin_session_id,
                        inbound_seq: origin_inbound_seq,
                        ..
                    } if origin_connection_id == connection_id
                        && origin_session_id == session_id
                        && *origin_inbound_seq == inbound_seq
                )
            })
            .map(|operation| operation.operation_id.clone());
        let operation_id = matches.next();
        if matches.next().is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Ok(operation_id)
    }

    pub(super) fn resolve_surface_jsonl_turn_operation(
        &self,
        client: &surface::RuntimeSurfaceClientHandle,
        legacy_turn_id: &surface::LegacyTurnId,
    ) -> Result<Option<surface::OperationRecord>, surface::SurfaceClientCommandError> {
        let connection_id = client
            .connection_id()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let mut matches = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .filter(|operation| match &operation.intent.origin {
                surface::OperationOrigin::JsonlThreadTurn {
                    connection_id: origin_connection_id,
                    legacy_turn_id: origin_turn_id,
                    ..
                } => origin_connection_id == connection_id && origin_turn_id == legacy_turn_id,
                surface::OperationOrigin::JsonlStatelessSubmit {
                    connection_id: origin_connection_id,
                    ..
                } => {
                    origin_connection_id == connection_id
                        && operation.initial_logical_turn_id.as_ref().is_some_and(
                            |logical_turn_id| {
                                logical_turn_id.to_string() == legacy_turn_id.0.as_str()
                            },
                        )
                }
                _ => false,
            })
            .cloned();
        let operation = matches.next();
        if matches.next().is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Ok(operation)
    }

    pub(super) fn committed_jsonl_turn_control(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        legacy_turn_id: surface::LegacyTurnId,
        action: &surface::JsonlTurnControlAction,
        status: surface::JsonlResolvedTurnControlStatus,
        batch: &surface::SurfaceCommitBatch,
        input_item_id: Option<surface::SurfaceItemId>,
        family: surface::SurfaceFactFamily,
    ) -> surface::JsonlTurnControlResult {
        let event = &batch.events.as_slice()[0];
        surface::JsonlTurnControlResult::Resolved {
            mutation: surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: batch.cursor_after.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::Accepted,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::ThreadLocalCursor {
                            cursor: batch.cursor_after.clone(),
                            family,
                            event_id: event.event_id.clone(),
                            commit_class: batch.commit_class.clone(),
                        },
                    ])
                    .expect("JSONL turn control commit has one acknowledgement"),
                },
                value: surface::JsonlTurnControlledOutput {
                    operation_id,
                    echo: surface::JsonlResolvedTurnControlWireEcho {
                        legacy_turn_id,
                        action: jsonl_turn_control_wire_action(action),
                        status,
                        legacy_input: jsonl_turn_control_legacy_input(action),
                    },
                    committed_cursor: batch.cursor_after.clone(),
                    input_item_id,
                },
            },
        }
    }

    pub(super) fn replayed_jsonl_turn_control(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        legacy_turn_id: surface::LegacyTurnId,
        action: &surface::JsonlTurnControlAction,
        status: surface::JsonlResolvedTurnControlStatus,
        acknowledgement: surface::MutationCommitAck,
    ) -> Result<surface::JsonlTurnControlResult, surface::SurfaceClientCommandError> {
        let surface::MutationCommitAck::ThreadLocalCursor { cursor, .. } = &acknowledgement else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let cursor = cursor.clone();
        Ok(surface::JsonlTurnControlResult::Resolved {
            mutation: surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: cursor.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![acknowledgement])
                        .expect("JSONL turn control replay has one acknowledgement"),
                },
                value: surface::JsonlTurnControlledOutput {
                    operation_id,
                    echo: surface::JsonlResolvedTurnControlWireEcho {
                        legacy_turn_id,
                        action: jsonl_turn_control_wire_action(action),
                        status,
                        legacy_input: jsonl_turn_control_legacy_input(action),
                    },
                    committed_cursor: cursor,
                    input_item_id: None,
                },
            },
        })
    }

    pub(super) fn dispatch_control_jsonl_turn_idle(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        legacy_turn_id: surface::LegacyTurnId,
        action: surface::JsonlTurnControlAction,
        reply: SyncSender<
            Result<surface::JsonlTurnControlResult, surface::SurfaceClientCommandError>,
        >,
    ) {
        if !self.admits_surface_client(&client, surface::SurfaceCapability::ControlBoundOperation)
            || client.grant().role != surface::SurfaceAttachmentRole::Jsonl
        {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::Unauthorized));
            return;
        }
        let operation = match self.resolve_surface_jsonl_turn_operation(&client, &legacy_turn_id) {
            Ok(Some(operation)) => operation,
            Ok(None) => {
                let _ = reply.send(Ok(jsonl_idle_turn_control(
                    request_id,
                    legacy_turn_id,
                    &action,
                )));
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let surface::JsonlTurnControlAction::Resume = action else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let Some(previous) = operation.generations.last() else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let resume_source = match &previous.replayability {
            surface::Replayability::Replayable { .. } => {
                surface::ResumeSourceWitness::DurableReplay {
                    replayability_digest: surface::canonical_replayability_digest(
                        &previous.replayability,
                    ),
                }
            }
            surface::Replayability::NonReplayable { .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::Unauthorized));
                return;
            }
        };
        let operation_id = operation.operation_id.clone();
        self.dispatch_surface_resume(
            client,
            request_id.clone(),
            operation.operation_id,
            previous.fence.generation_id,
            resume_source,
            move |_actor, resumed| {
                let mapped = resumed.map(|resumed| match resumed {
                    surface::MutationReply::Committed { mutation, value } => {
                        surface::MutationReply::Committed {
                            mutation,
                            value: surface::JsonlTurnControlledOutput {
                                operation_id,
                                echo: surface::JsonlResolvedTurnControlWireEcho {
                                    legacy_turn_id,
                                    action: surface::JsonlTurnControlWireAction::Resume,
                                    status: surface::JsonlResolvedTurnControlStatus::Resumed,
                                    legacy_input: None,
                                },
                                committed_cursor: value.generation_started.cursor,
                                input_item_id: None,
                            },
                        }
                    }
                    surface::MutationReply::Deferred { mutation, partial } => {
                        surface::MutationReply::Deferred {
                            mutation,
                            partial: match partial {
                                surface::DeferredCommandValue::NoValue => {
                                    surface::DeferredCommandValue::NoValue
                                }
                                surface::DeferredCommandValue::Provisional { value } => {
                                    surface::DeferredCommandValue::Provisional {
                                        value: surface::JsonlTurnControlledOutput {
                                            operation_id,
                                            echo: surface::JsonlResolvedTurnControlWireEcho {
                                                legacy_turn_id,
                                                action:
                                                    surface::JsonlTurnControlWireAction::Resume,
                                                status: surface::JsonlResolvedTurnControlStatus::Resumed,
                                                legacy_input: None,
                                            },
                                            committed_cursor: value.generation_started.cursor,
                                            input_item_id: None,
                                        },
                                    }
                                }
                            },
                        }
                    }
                    surface::MutationReply::Uncommitted { mutation } => {
                        surface::MutationReply::Uncommitted { mutation }
                    }
                });
                let _ = reply.send(mapped.map(|mutation| {
                    surface::JsonlTurnControlResult::Resolved { mutation }
                }));
            },
        );
    }

    pub(super) fn control_jsonl_turn_running(
        &mut self,
        active: &mut ActiveOperation,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        legacy_turn_id: surface::LegacyTurnId,
        action: surface::JsonlTurnControlAction,
    ) -> Result<surface::JsonlTurnControlResult, surface::SurfaceClientCommandError> {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ControlBoundOperation)
            || client.grant().role != surface::SurfaceAttachmentRole::Jsonl
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let Some(operation) = self.resolve_surface_jsonl_turn_operation(client, &legacy_turn_id)?
        else {
            return Ok(jsonl_idle_turn_control(request_id, legacy_turn_id, &action));
        };
        let fence = active
            .surface_operation
            .as_ref()
            .filter(|fence| fence.operation_id == operation.operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        match &action {
            surface::JsonlTurnControlAction::Interrupt => {
                let intent = surface::PendingControlIntent::Interrupt {
                    generation_fence: fence.clone(),
                };
                if let Some(acknowledgement) = self
                    .resident_surface
                    .coordinator
                    .state()
                    .control_intent_acknowledgement(&operation.operation_id, &intent)
                {
                    return Self::replayed_jsonl_turn_control(
                        request_id,
                        operation.operation_id,
                        legacy_turn_id,
                        &action,
                        surface::JsonlResolvedTurnControlStatus::Interrupted,
                        acknowledgement,
                    );
                }
                if active.generation.cancel.is_cancelled() {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
                let mut interactions = self
                    .resident_surface
                    .interactions
                    .iter()
                    .filter(|(_, interaction)| {
                        interaction.record.fence == fence
                            && interaction.winning_receipt.is_none()
                            && interaction.cancelled.is_none()
                            && interaction.private_response.is_none()
                    })
                    .map(|(interaction_id, interaction)| {
                        (interaction_id.clone(), interaction.revision)
                    })
                    .collect::<Vec<_>>();
                interactions.sort_by_key(|(interaction_id, _)| interaction_id.clone());
                let mut events = vec![(
                    surface::SurfaceScope::Operation {
                        operation_id: operation.operation_id.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            operation_id: operation.operation_id.clone(),
                            request_id: operation.request_id,
                            intent,
                        },
                    ),
                )];
                for (interaction_id, expected_revision) in &interactions {
                    let interaction_scope = snapshot
                        .interactions
                        .iter()
                        .find(|interaction| &interaction.interaction_id == interaction_id)
                        .filter(|interaction| {
                            surface::detached_child_permission_interaction_matches(
                                &snapshot,
                                interaction,
                                true,
                                false,
                            ) || surface::detached_child_permission_interaction_terminal_matches(
                                &snapshot,
                                interaction,
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
                            next_revision: surface::InteractionRevision::try_new(
                                expected_revision
                                    .get()
                                    .checked_add(1)
                                    .expect("interaction revision did not exhaust"),
                            )
                            .expect("interaction revision did not exhaust"),
                            reason: surface::InteractionCancelReason::OperationCancelled {
                                reason: surface::CancelReason::User,
                            },
                        }),
                    ));
                }
                let batch = self.surface_event_batch_with_commit_id(events, None);
                self.resident_surface
                    .coordinator
                    .commit_actor_generation_interrupt_batch(fence, &batch)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                self.apply_surface_interaction_cancellations(
                    &interactions
                        .into_iter()
                        .map(|(interaction_id, _)| interaction_id)
                        .collect::<Vec<_>>(),
                );
                Self::cancel_active_task_tree(active);
                Ok(Self::committed_jsonl_turn_control(
                    request_id,
                    operation.operation_id,
                    legacy_turn_id,
                    &action,
                    surface::JsonlResolvedTurnControlStatus::Interrupted,
                    &batch,
                    None,
                    surface::SurfaceFactFamily::Operation,
                ))
            }
            surface::JsonlTurnControlAction::Resume => {
                let intent = surface::PendingControlIntent::ResumeAfterInterruptedStop {
                    generation_fence: fence.clone(),
                };
                if let Some(acknowledgement) = self
                    .resident_surface
                    .coordinator
                    .state()
                    .control_intent_acknowledgement(&operation.operation_id, &intent)
                {
                    return Self::replayed_jsonl_turn_control(
                        request_id,
                        operation.operation_id,
                        legacy_turn_id,
                        &action,
                        surface::JsonlResolvedTurnControlStatus::Resumed,
                        acknowledgement,
                    );
                }
                if !active.generation.cancel.is_cancelled()
                    || !matches!(
                        operation.pending_control,
                        Some(surface::PendingControlIntent::Interrupt {
                            generation_fence: ref interrupted,
                        }) if interrupted == &fence
                    )
                {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                let batch = self.surface_operation_batch(
                    &operation.operation_id,
                    vec![surface::OperationPatch::ControlIntentCommitted {
                        operation_id: operation.operation_id.clone(),
                        request_id: operation.request_id,
                        intent,
                    }],
                );
                self.resident_surface
                    .coordinator
                    .commit_actor_batch(&batch)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                Ok(Self::committed_jsonl_turn_control(
                    request_id,
                    operation.operation_id,
                    legacy_turn_id,
                    &action,
                    surface::JsonlResolvedTurnControlStatus::Resumed,
                    &batch,
                    None,
                    surface::SurfaceFactFamily::Operation,
                ))
            }
            surface::JsonlTurnControlAction::Steer { input } => {
                if active.generation.cancel.is_cancelled()
                    || active.generation.join.is_finished()
                    || active.resume_queued
                {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                let resolved = resolve_surface_input(input)
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                let persisted = surface_input_for_persisted_presentation(&resolved);
                let input_item_id = surface::SurfaceItemId::new();
                let batch = self.surface_event_batch_with_commit_id(
                    vec![(
                        surface::SurfaceScope::Generation {
                            fence: fence.clone(),
                        },
                        surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                            item: surface::SurfaceItem::UserMessage {
                                id: input_item_id.clone(),
                                turn_id: operation
                                    .generations
                                    .last()
                                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?
                                    .logical_turn_id
                                    .clone(),
                                input: surface::SurfaceUserInputState::Resolved {
                                    fact: surface::SurfaceResolvedInputFact::Replayable {
                                        input: persisted,
                                        request_digest: surface_sha256(
                                            &serde_json::to_vec(input).expect(
                                                "JSONL steer input request is serializable",
                                            ),
                                        ),
                                    },
                                },
                                pinned: false,
                                origin: surface::SurfaceItemOrigin::UserInput,
                            },
                        }),
                    )],
                    None,
                );
                self.commit_surface_generation_batch_with_retry(fence, &batch)
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                active.steer_handle.push(resolved.canonical_text.as_str());
                Ok(Self::committed_jsonl_turn_control(
                    request_id,
                    operation.operation_id,
                    legacy_turn_id,
                    &action,
                    surface::JsonlResolvedTurnControlStatus::Steered,
                    &batch,
                    Some(input_item_id),
                    surface::SurfaceFactFamily::Item,
                ))
            }
        }
    }

    pub(super) fn cancel_surface_acp_prompt_idle(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        session_id: surface::NonEmptyText,
        inbound_seq: surface::SequenceNumber,
    ) -> Result<surface::CancelSessionCurrentResult, surface::SurfaceClientCommandError> {
        let Some(operation_id) =
            self.resolve_surface_acp_prompt_operation(client, &session_id, inbound_seq)?
        else {
            return Ok(surface::CancelSessionCurrentResult::NoCurrentOperation {
                request_id,
                thread_id: self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .thread
                    .thread_id
                    .clone(),
            });
        };
        self.cancel_surface_idle(client, request_id, operation_id)
            .map(|mutation| surface::CancelSessionCurrentResult::Resolved { mutation })
    }

    pub(super) fn cancel_surface_background_provider(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        snapshot: &surface::SurfaceSnapshot,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.cancel_surface_background_provider_with_batch(request_id, operation_id, snapshot)
            .map(|(reply, _)| reply)
    }

    pub(super) fn cancel_surface_background_provider_with_batch(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        snapshot: &surface::SurfaceSnapshot,
    ) -> Result<
        (
            surface::MutationReply<surface::CancelOperationOutput>,
            surface::SurfaceCommitBatch,
        ),
        surface::SurfaceClientCommandError,
    > {
        let typed = self
            .background_controller
            .find_provider_matching(|typed| {
                typed.fence.operation_fence.operation_id == operation_id
            })
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let operation = snapshot
            .operation_history
            .iter()
            .find(|operation| {
                operation.operation_id == operation_id && operation.terminal.is_none()
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == typed.task_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if task.status == surface::SurfaceTaskStatus::Stopping {
            self.background_controller
                .cancel_task(typed.task_id.as_str());
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let next_task_revision = surface::TaskRevision::try_new(
            task.revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Background {
                        fence: typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            operation_id: operation_id.clone(),
                            request_id: operation.request_id,
                            intent: surface::PendingControlIntent::Terminalize {
                                operation_id: operation_id.clone(),
                                cause: surface::TerminalizationCause::UserCancel,
                            },
                        },
                    ),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                        task_id: task.task_id,
                        expected_revision: task.revision,
                        next_revision: next_task_revision,
                        status: surface::SurfaceTaskStatus::Stopping,
                        completed_at: None,
                        result: None,
                        error: None,
                    }),
                ),
            ],
            None,
        );
        let pending = PendingSurfaceBackgroundControl {
            fence: typed.fence.clone(),
            batch: batch.clone(),
            task_id: typed.task_id.clone(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        };
        match self
            .resident_surface
            .coordinator
            .commit_actor_background_control_batch(typed.fence, &batch)
        {
            Ok(_) => self.apply_committed_surface_background_control(&pending),
            Err(
                surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::CheckpointFailed)
                | surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::PartialAppend),
            ) => {
                self.background_controller
                    .retain_control(operation_id, pending);
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            Err(_) => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        }
        Ok((
            Self::committed_surface_mutation(
                request_id,
                operation_id.clone(),
                &batch,
                surface::CancelOperationOutput::Accepted {
                    operation_id,
                    accepted_cursor: batch.cursor_after.clone(),
                    waiter: surface::OperationWaiterHandle::new(),
                },
            ),
            batch,
        ))
    }

    pub(super) fn cancel_surface_background_workflow(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        snapshot: &surface::SurfaceSnapshot,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        let typed = self
            .background_controller
            .find_workflow_matching(|typed| {
                typed.fence.operation_fence.operation_id == operation_id
            })
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let operation = snapshot
            .operation_history
            .iter()
            .find(|operation| {
                operation.operation_id == operation_id && operation.terminal.is_none()
            })
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == typed.task_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let workflow = snapshot
            .workflows
            .iter()
            .find(|workflow| workflow.workflow_run_id == typed.workflow_run_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if task.status == surface::SurfaceTaskStatus::Stopping
            && workflow.status == surface::SurfaceWorkflowStatus::Stopping
        {
            self.background_controller
                .cancel_task(typed.task_id.as_str());
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let next_task_revision = surface::TaskRevision::try_new(
            task.revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let next_workflow_revision = surface::WorkflowRevision::try_new(
            workflow
                .revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let workflow_fence = surface::SurfaceWorkflowFence {
            workflow_run_id: workflow.workflow_run_id.clone(),
            workflow_revision: workflow.revision,
            parent: workflow.parent.clone(),
        };
        let reason = surface::DisplayText::new("Workflow cancellation requested");
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Background {
                        fence: typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            operation_id: operation_id.clone(),
                            request_id: operation.request_id,
                            intent: surface::PendingControlIntent::Terminalize {
                                operation_id: operation_id.clone(),
                                cause: surface::TerminalizationCause::UserCancel,
                            },
                        },
                    ),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                        task_id: task.task_id,
                        expected_revision: task.revision,
                        next_revision: next_task_revision,
                        status: surface::SurfaceTaskStatus::Stopping,
                        completed_at: None,
                        result: None,
                        error: None,
                    }),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Workflow(surface::WorkflowPatch::Stopping {
                        fence: workflow_fence,
                        next_revision: next_workflow_revision,
                        reason,
                    }),
                ),
            ],
            None,
        );
        let pending = PendingSurfaceBackgroundControl {
            fence: typed.fence.clone(),
            batch: batch.clone(),
            task_id: typed.task_id.clone(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        };
        match self
            .resident_surface
            .coordinator
            .commit_actor_background_control_batch(typed.fence.clone(), &batch)
        {
            Ok(_) => self.apply_committed_surface_background_control(&pending),
            Err(
                surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::CheckpointFailed)
                | surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::PartialAppend),
            ) => {
                self.background_controller
                    .retain_control(operation_id, pending);
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            Err(_) => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        }
        let operation_event = &batch.events.as_slice()[0];
        let task_event = &batch.events.as_slice()[1];
        let workflow_event = &batch.events.as_slice()[2];
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Operation {
                    thread_id: batch.cursor_after.thread_id.clone(),
                    operation_id: operation_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Operation,
                        event_id: operation_event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Task,
                        event_id: task_event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Workflow,
                        event_id: workflow_event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("workflow cancellation commits task and workflow facts"),
            },
            value: surface::CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor: batch.cursor_after,
                waiter: surface::OperationWaiterHandle::new(),
            },
        })
    }

    pub(super) fn retry_surface_background_control(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = BackgroundRetryKey::Control(operation_id.clone());
        let Some(BackgroundRetryEffect::Control {
            operation_id,
            pending,
        }) = self.background_controller.begin_retry(&key)
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_actor_background_control_batch(pending.fence.clone(), &pending.batch)
            .is_err()
        {
            self.background_controller.resolve_retry(
                BackgroundRetryEffect::Control {
                    operation_id,
                    pending,
                },
                BackgroundRetryResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                ),
            );
            return;
        }
        self.apply_committed_surface_background_control(&pending);
        self.background_controller.resolve_retry(
            BackgroundRetryEffect::Control {
                operation_id,
                pending,
            },
            BackgroundRetryResolution::Settled,
        );
    }

    pub(super) fn settle_surface_background_controls_for_shutdown(&mut self) -> bool {
        for _ in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            if !self.background_controller.has_pending_control() {
                return true;
            }
            let operation_ids = self.background_controller.pending_control_operation_ids();
            for operation_id in operation_ids {
                self.retry_surface_background_control(&operation_id);
            }
        }
        !self.background_controller.has_pending_control()
    }

    pub(super) fn apply_committed_surface_background_control(
        &mut self,
        pending: &PendingSurfaceBackgroundControl,
    ) {
        if let Some(state) = self.state.as_ref() {
            let _ = state
                .thread
                .session()
                .task_registry()
                .request_stop(pending.task_id.as_str());
        }
        self.background_controller
            .cancel_task(pending.task_id.as_str());
    }

    pub(super) fn cancel_surface_running(
        &mut self,
        active: &mut ActiveOperation,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self
            .resident_surface
            .interactions
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let fence = active
            .surface_operation
            .as_ref()
            .filter(|fence| fence.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let batch = self.commit_surface_terminalization_batch(
            active,
            &fence,
            surface::TerminalizationCause::UserCancel,
        )?;
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &batch,
            surface::CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor: batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    pub(super) fn dispatch_pause_goal_surface_running(
        &mut self,
        active: &mut ActiveOperation,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        goal_fence: surface::SurfaceGoalFence,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::PauseGoalOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let operation_fence = active
                .surface_operation
                .as_ref()
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            if active.surface_terminalization.is_some()
                || !active.request.surface_goal_owned
                || !self.bind_surface_operation_controller(&client, &operation_fence.operation_id)
            {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let goal = snapshot
                .goal
                .as_ref()
                .filter(|goal| {
                    goal.goal_id == goal_fence.goal_id
                        && goal.goal_revision == goal_fence.goal_revision
                        && goal.goal_owner_epoch == goal_fence.goal_owner_epoch
                        && goal.current_run.as_ref().is_some_and(|run| {
                            run.operation_id == operation_fence.operation_id
                                && matches!(
                                    run.phase,
                                    surface::SurfaceGoalRunPhase::InFlight { .. }
                                )
                        })
                })
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let operation_request_id = snapshot
                .foreground_operation
                .as_ref()
                .filter(|operation| operation.operation_id == operation_fence.operation_id)
                .map(|operation| operation.request_id.clone())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let control = self
                .goal_controller
                .active_control(active.operation_id)
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&(
                    "pause_goal_operation",
                    request_id.as_bytes(),
                    &goal_fence,
                    &operation_fence,
                ))
                .expect("Goal pause digest input is serializable"),
            )
            .as_bytes();
            Ok((
                control.runtime,
                PauseGoalForSurfaceInput {
                    session_id: control.session_id,
                    expected_goal_id: orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
                        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    expected_goal_revision: u32::try_from(goal.goal_revision.get())
                        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                    expected_operation_id: operation_fence.operation_id.clone(),
                    message: "paused by user".to_string(),
                    paused_at: chrono::Utc::now().timestamp(),
                },
                GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest,
                    goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                },
                operation_fence,
                operation_request_id,
            ))
        })();
        let (runtime, input, context, operation_fence, operation_request_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            "running Goal pause",
            GoalBlockingCompletionKind::PauseResume,
            move || prepare_running_goal_pause_worker(runtime, input, context),
            move |actor, result| {
                let result = result
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)
                    .and_then(|worker| {
                        actor.settle_goal_surface_running_pause(
                            request_id,
                            operation_fence,
                            operation_request_id,
                            worker,
                        )
                    });
                let _ = reply.send(result);
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn settle_goal_surface_running_pause(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_fence: surface::SurfaceOperationFence,
        operation_request_id: surface::SurfaceRequestId,
        worker: GoalSurfaceWorkerResult,
    ) -> Result<surface::MutationReply<surface::PauseGoalOutput>, surface::SurfaceClientCommandError>
    {
        let GoalSurfaceWorkerResult {
            runtime,
            mut mutations,
            ..
        } = worker;
        let mutation = mutations
            .pop()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let active_matches = self.active.as_ref().is_some_and(|active| {
            active
                .surface_operation
                .as_ref()
                .is_some_and(|fence| fence == &operation_fence)
                && active.request.surface_goal_owned
                && active.surface_terminalization.is_none()
        });
        if !active_matches {
            self.settle_goal_surface_mutation(&runtime, &mutation)
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let mut active = self
            .active
            .take()
            .expect("running Goal pause active operation was prevalidated");
        let result = (|| {
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let (committed_goal_fence, receipt_digest, _, goal_scope, goal_event) =
                surface_goal_mutation_event(&mutation, snapshot.thread.thread_id.clone())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_receipt = match &goal_event {
                surface::SurfaceEvent::Goal(envelope) => envelope.receipt.clone(),
                _ => unreachable!("Goal pause projects a Goal event"),
            };
            let batch = self.surface_event_batch_with_commit_id(
                vec![
                    (goal_scope, goal_event),
                    (
                        surface::SurfaceScope::Operation {
                            operation_id: operation_fence.operation_id.clone(),
                        },
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::ControlIntentCommitted {
                                operation_id: operation_fence.operation_id.clone(),
                                request_id: operation_request_id,
                                intent: surface::PendingControlIntent::Terminalize {
                                    operation_id: operation_fence.operation_id.clone(),
                                    cause: surface::TerminalizationCause::GoalPause,
                                },
                            },
                        ),
                    ),
                ],
                None,
            );
            self.resident_surface
                .coordinator
                .commit_actor_goal_batch(committed_goal_fence, receipt_digest, &batch)
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let acknowledgement = mutation.clone();
            tokio::task::spawn_blocking(move || {
                Self::acknowledge_goal_surface_mutation_best_effort(&runtime, &acknowledgement);
            });
            active.surface_terminalization = Some(surface::TerminalizationCause::GoalPause);
            active.generation.cancel.cancel();
            let goal = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .goal
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_event_id = batch.events.as_slice()[0].event_id.clone();
            Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Goal {
                        goal_id: goal.goal_id.clone(),
                    },
                    disposition: surface::MutationDisposition::Accepted,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::ThreadLocalCursor {
                            cursor: batch.cursor_after.clone(),
                            family: surface::SurfaceFactFamily::Goal,
                            event_id: goal_event_id,
                            commit_class: batch.commit_class.clone(),
                        },
                    ])
                    .expect("Goal pause has one acknowledgement"),
                },
                value: surface::PauseGoalOutput {
                    goal,
                    goal_receipt,
                    goal_cursor: batch.cursor_after.clone(),
                    operation: surface::PauseGoalOperationOutput::Cancelling {
                        operation_id: operation_fence.operation_id,
                        accepted_cursor: batch.cursor_after.clone(),
                        waiter: surface::OperationWaiterHandle::new(),
                    },
                },
            })
        })();
        self.active = Some(active);
        result
    }

    pub(super) fn dispatch_pause_goal_surface_idle(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        goal_fence: surface::SurfaceGoalFence,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::PauseGoalOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        if self.operation_recovery.pending_manual_compaction.is_some() {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        }
        let Some(session_id) = self.handle.session_id.clone() else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let Some(goal) = snapshot
            .goal
            .as_ref()
            .filter(|goal| {
                goal.goal_id == goal_fence.goal_id
                    && goal.goal_revision == goal_fence.goal_revision
                    && goal.goal_owner_epoch == goal_fence.goal_owner_epoch
                    && goal.current_run.is_none()
                    && !matches!(goal.state, surface::SurfaceGoalState::Complete { .. })
            })
            .cloned()
        else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let Some(runtime) = self
            .state
            .as_ref()
            .and_then(|state| state.thread.initialized_goal_runtime_handle())
        else {
            self.goal_controller
                .defer(ThreadCommand::SurfacePauseGoalOperation {
                    client,
                    request_id,
                    goal_fence,
                    reply,
                });
            let (open_reply, _receive) = mpsc::sync_channel(1);
            self.open_goal_runtime_off_actor(open_reply);
            return;
        };
        let command_digest = *surface_sha256(
            &serde_json::to_vec(&(
                "pause_quiescent_goal_operation",
                request_id.as_bytes(),
                &goal_fence,
            ))
            .expect("quiescent Goal pause digest input is serializable"),
        )
        .as_bytes();
        let Ok(expected_goal_id) = orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
        else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let Ok(expected_goal_revision) = u32::try_from(goal.goal_revision.get()) else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        let input = PauseQuiescentGoalForSurfaceInput {
            session_id: session_id.clone(),
            expected_goal_id,
            expected_goal_revision,
            message: "paused by user".to_string(),
            paused_at: chrono::Utc::now().timestamp(),
        };
        let context = GoalSurfaceMutationContext {
            store_commit_id: uuid::Uuid::now_v7().to_string(),
            command_digest,
            goal_owner_epoch: snapshot.thread.owner_epoch.get(),
        };
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            "quiescent Goal pause",
            GoalBlockingCompletionKind::PauseResume,
            move || prepare_quiescent_goal_pause_worker(runtime, session_id, input, context),
            move |actor, result| {
                let result = result
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)
                    .and_then(|worker| actor.settle_goal_surface_idle_pause(request_id, worker));
                let _ = reply.send(result);
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn settle_goal_surface_idle_pause(
        &mut self,
        request_id: surface::SurfaceRequestId,
        worker: GoalSurfaceWorkerResult,
    ) -> Result<surface::MutationReply<surface::PauseGoalOutput>, surface::SurfaceClientCommandError>
    {
        let mutation = worker
            .mutations
            .last()
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let (_, batches) = self
            .settle_goal_surface_worker_with_batches(worker)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = batches
            .into_iter()
            .last()
            .flatten()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let (_, _, _, _, event) =
            surface_goal_mutation_event(&mutation, batch.cursor_after.thread_id.clone())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let surface::SurfaceEvent::Goal(goal_event) = event else {
            unreachable!("quiescent Goal pause projects a Goal event")
        };
        let goal = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .goal
            .clone()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let goal_event_id = batch.events.as_slice()[0].event_id.clone();
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Goal {
                    goal_id: goal.goal_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Goal,
                        event_id: goal_event_id,
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("quiescent Goal pause has one acknowledgement"),
            },
            value: surface::PauseGoalOutput {
                goal,
                goal_receipt: goal_event.receipt,
                goal_cursor: batch.cursor_after,
                operation: surface::PauseGoalOperationOutput::None,
            },
        })
    }

    pub(super) fn commit_surface_terminalization(
        &mut self,
        active: &mut ActiveOperation,
        cause: surface::TerminalizationCause,
    ) -> Result<(), RuntimeHostError> {
        let Some(fence) = active.surface_operation.clone() else {
            return Ok(());
        };
        self.commit_surface_terminalization_batch(active, &fence, cause)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed shutdown intent: {error:?}"),
            })?;
        Ok(())
    }

    pub(super) fn commit_surface_terminalization_batch(
        &mut self,
        active: &mut ActiveOperation,
        fence: &surface::SurfaceOperationFence,
        cause: surface::TerminalizationCause,
    ) -> Result<surface::SurfaceCommitBatch, surface::SurfaceClientCommandError> {
        if !self.settle_surface_capability_transitions_for_shutdown() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if self.resident_surface.capability.any_claimed_durable_call(
            self.resident_surface.coordinator.state().snapshot(),
            |call| Self::surface_capability_write_blocks_terminalization(call, fence),
        ) {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if active.surface_terminalization.is_some()
            || !self
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
        active.surface_terminalization = Some(cause);
        let prepared = (|| {
            self.drain_private_surface_interactions(fence)?;
            let original_request_id = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .foreground_operation
                .as_ref()
                .map(|operation| operation.request_id.clone())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            self.prepare_surface_terminalization(fence, original_request_id, cause)
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                active.surface_terminalization = None;
                return Err(error);
            }
        };
        match self
            .resident_surface
            .coordinator
            .commit_actor_generation_terminalization_batch(fence.clone(), &prepared.batch)
        {
            Ok(_) => {
                Self::cancel_active_task_tree(active);
                self.apply_surface_interaction_cancellations(&prepared.interaction_ids);
                self.apply_surface_capability_cancellations(&prepared.capability_call_ids);
                Ok(prepared.batch)
            }
            Err(
                surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::CheckpointFailed)
                | surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::PartialAppend),
            ) => {
                if self
                    .resident_surface
                    .commit
                    .prepare_terminalization(prepared)
                    .is_err()
                {
                    active.surface_terminalization = None;
                }
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            }
            Err(_) => {
                active.surface_terminalization = None;
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            }
        }
    }

    pub(super) fn surface_capability_write_blocks_terminalization(
        call: &surface::SurfaceCapabilityCall,
        fence: &surface::SurfaceOperationFence,
    ) -> bool {
        call.fence == *fence
            && !matches!(
                (call.kind, &call.state),
                (
                    surface::SurfaceCapabilityCallKind::TerminalKill
                        | surface::SurfaceCapabilityCallKind::TerminalRelease,
                    surface::SurfaceCapabilityCallState::DeliveryPossible
                        | surface::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                )
            )
    }

    pub(super) fn surface_interaction_admission_closed(active: &ActiveOperation) -> bool {
        active.surface_terminalization.is_some()
            || active.surface_execution_failure.is_some()
            || active.generation.cancel.is_cancelled()
    }
}
