// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

impl ThreadActor {
    pub(super) fn reject_goal_surface_command(command: ThreadCommand, error: RuntimeHostError) {
        match command {
            ThreadCommand::SetGoal { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            ThreadCommand::EditGoal { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            ThreadCommand::ClearGoal { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            _ => unreachable!("only Goal surface commands reach this helper"),
        }
    }

    pub(super) fn dispatch_goal_surface_command(&mut self, command: ThreadCommand) {
        if self.state.is_none() {
            Self::reject_goal_surface_command(command, RuntimeHostError::ThreadUnavailable);
            return;
        }
        let command_session_id = match &command {
            ThreadCommand::SetGoal { session_id, .. }
            | ThreadCommand::EditGoal { session_id, .. }
            | ThreadCommand::ClearGoal { session_id, .. } => session_id.as_str(),
            _ => unreachable!("only Goal surface commands reach the worker"),
        };
        if self.handle.session_id.as_deref() != Some(command_session_id) {
            Self::reject_goal_surface_command(
                command,
                RuntimeHostError::GoalControlFailed {
                    message: "Goal mutation does not belong to this runtime thread".to_string(),
                },
            );
            return;
        }
        let Some(runtime) = self
            .state
            .as_ref()
            .and_then(|state| state.thread.initialized_goal_runtime_handle())
        else {
            self.goal_controller.defer(command);
            let (reply, _receive) = mpsc::sync_channel(1);
            self.open_goal_runtime_off_actor(reply);
            return;
        };
        if !self.goal_controller.begin_blocking() {
            self.goal_controller.defer(command);
            return;
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let completion_tx = self.goal_controller.completion_sender();
        tokio::spawn(async move {
            let completion = match command {
                ThreadCommand::SetGoal {
                    session_id,
                    objective,
                    at,
                    reply,
                } => {
                    let result = tokio::task::spawn_blocking(move || {
                        prepare_goal_surface_worker(
                            runtime,
                            snapshot,
                            GoalSurfaceCommand::Set {
                                session_id,
                                objective,
                                at,
                            },
                        )
                    })
                    .await
                    .map_err(|error| RuntimeHostError::GoalControlFailed {
                        message: format!("goal surface worker failed: {error}"),
                    })
                    .and_then(|result| result);
                    GoalBlockingCompletion::SetGoal { reply, result }
                }
                ThreadCommand::EditGoal {
                    session_id,
                    objective,
                    at,
                    reply,
                } => {
                    let result = tokio::task::spawn_blocking(move || {
                        prepare_goal_surface_worker(
                            runtime,
                            snapshot,
                            GoalSurfaceCommand::Edit {
                                session_id,
                                objective,
                                at,
                            },
                        )
                    })
                    .await
                    .map_err(|error| RuntimeHostError::GoalControlFailed {
                        message: format!("goal surface worker failed: {error}"),
                    })
                    .and_then(|result| result);
                    GoalBlockingCompletion::EditGoal { reply, result }
                }
                ThreadCommand::ClearGoal { session_id, reply } => {
                    let result = tokio::task::spawn_blocking(move || {
                        prepare_goal_surface_worker(
                            runtime,
                            snapshot,
                            GoalSurfaceCommand::Clear { session_id },
                        )
                    })
                    .await
                    .map_err(|error| RuntimeHostError::GoalControlFailed {
                        message: format!("goal surface worker failed: {error}"),
                    })
                    .and_then(|result| result);
                    GoalBlockingCompletion::ClearGoal { reply, result }
                }
                _ => unreachable!("only Goal surface commands reach the worker"),
            };
            let _ = completion_tx.send(completion).await;
        });
    }

    pub(super) fn settle_goal_surface_worker(
        &mut self,
        worker: GoalSurfaceWorkerResult,
    ) -> Result<Option<orca_core::goal_types::ThreadGoal>, RuntimeHostError> {
        self.settle_goal_surface_worker_with_batches(worker)
            .map(|(goal, _)| goal)
    }

    pub(super) fn settle_goal_surface_worker_with_batches(
        &mut self,
        worker: GoalSurfaceWorkerResult,
    ) -> Result<
        (
            Option<orca_core::goal_types::ThreadGoal>,
            Vec<Option<surface::SurfaceCommitBatch>>,
        ),
        RuntimeHostError,
    > {
        let GoalSurfaceWorkerResult {
            runtime,
            mutations,
            projected_goal,
        } = worker;
        let mut batches = Vec::with_capacity(mutations.len());
        for mutation in &mutations {
            batches.push(self.commit_goal_surface_mutation_with_retry(mutation)?);
        }
        let acknowledgements = mutations.clone();
        tokio::task::spawn_blocking(move || {
            for mutation in acknowledgements {
                Self::acknowledge_goal_surface_mutation_best_effort(&runtime, &mutation);
            }
        });
        Ok((projected_goal, batches))
    }

    pub(super) fn settle_typed_goal_surface_worker(
        &mut self,
        worker: TypedGoalSurfaceWorkerResult,
    ) -> Result<(Vec<GoalSurfaceMutationRecord>, surface::SurfaceCommitBatch), RuntimeHostError>
    {
        let TypedGoalSurfaceWorkerResult {
            runtime,
            mutations,
            primary_start,
            commit,
        } = worker;
        for mutation in &mutations[..primary_start] {
            self.commit_goal_surface_mutation_with_retry(mutation)?;
        }
        let primary = mutations[primary_start..].to_vec();
        let batch = match commit {
            TypedGoalSurfaceCommit::Single => {
                let [mutation] = primary.as_slice() else {
                    return Err(RuntimeHostError::GoalControlFailed {
                        message: "Goal Store worker returned an invalid mutation count".to_string(),
                    });
                };
                self.commit_goal_surface_mutation_with_retry(mutation)?
                    .ok_or_else(|| RuntimeHostError::GoalControlFailed {
                        message: "typed Goal mutation was already acknowledged unexpectedly"
                            .to_string(),
                    })?
            }
            TypedGoalSurfaceCommit::EditAndRun => {
                let [edited, started] = primary.as_slice() else {
                    return Err(RuntimeHostError::GoalControlFailed {
                        message: "Goal edit-and-run worker returned an invalid mutation count"
                            .to_string(),
                    });
                };
                self.commit_goal_edit_and_run_with_retry(edited, started)?
            }
        };
        tokio::task::spawn_blocking(move || {
            for mutation in mutations {
                Self::acknowledge_goal_surface_mutation_best_effort(&runtime, &mutation);
            }
        });
        Ok((primary, batch))
    }

    pub(super) fn dispatch_typed_goal_run_preparation(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        worker: TypedGoalSurfaceWorkerResult,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let (primary, batch) = match self.settle_typed_goal_surface_worker(worker) {
            Ok(settled) => settled,
            Err(_) => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                return;
            }
        };
        let Some(mutation) = primary.last().cloned() else {
            let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            return;
        };
        self.resident_surface
            .interactions
            .operation_origin_attachments
            .insert(operation_id.clone(), client.attachment_id().clone());
        let (prepared, goal_work) = match self.prepare_surface_admission(
            &client,
            surface::SurfaceRequestId::new(),
            operation_id.clone(),
            admission_lease_id,
            None,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let dispatched = self.dispatch_prepared_surface_admission(
            prepared,
            goal_work,
            move |actor, admission| {
                let result = admission.and_then(|admission| {
                    let (admitted_cursor, waiter) = match admission {
                        surface::MutationReply::Committed {
                            value:
                                surface::AdmissionOutput::Admitted {
                                    admitted_cursor,
                                    waiter,
                                    ..
                                },
                            ..
                        } => (admitted_cursor, waiter),
                        _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
                    };
                    let mut mutation_reply = actor.goal_mutation_reply(
                        request_id,
                        &mutation,
                        &batch,
                        Some(operation_id),
                        Some(waiter),
                    )?;
                    if let surface::MutationReply::Committed { value, .. } = &mut mutation_reply {
                        value.goal = actor
                            .resident_surface
                            .coordinator
                            .state()
                            .snapshot()
                            .goal
                            .clone();
                        value.change_cursor = admitted_cursor;
                    }
                    Ok(mutation_reply)
                });
                let _ = reply.send(result);
            },
        );
        if dispatched.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn spawn_goal_blocking<ResultValue, Work, Settle>(
        &mut self,
        operation: &'static str,
        kind: GoalBlockingCompletionKind,
        work: Work,
        settle: Settle,
    ) -> Result<(), RuntimeHostError>
    where
        ResultValue: Send + 'static,
        Work: FnOnce() -> Result<ResultValue, RuntimeHostError> + Send + 'static,
        Settle: FnOnce(&mut ThreadActor, Result<ResultValue, RuntimeHostError>) + Send + 'static,
    {
        if !self.goal_controller.begin_blocking() {
            return Err(RuntimeHostError::GoalControlFailed {
                message: "another Goal Store request is still in flight".to_string(),
            });
        }
        let completion_tx = self.goal_controller.completion_sender();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(work)
                .await
                .map_err(|error| RuntimeHostError::GoalControlFailed {
                    message: format!("{operation} worker failed: {error}"),
                })
                .and_then(|result| result);
            let settlement: GoalBlockingSettlement = Box::new(move |actor| settle(actor, result));
            let _ = completion_tx.send(kind.completion(settlement)).await;
        });
        Ok(())
    }

    pub(super) fn dispatch_goal_pause(
        &mut self,
        operation_id: OperationId,
        message: &str,
    ) -> Result<(), RuntimeHostError> {
        self.dispatch_goal_pause_with_reason(
            operation_id,
            orca_core::goal_runtime::GoalPauseReason::User,
            message,
        )
    }

    pub(super) fn dispatch_goal_pause_with_reason(
        &mut self,
        operation_id: OperationId,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: &str,
    ) -> Result<(), RuntimeHostError> {
        let Some(control) = self.goal_controller.active_control(operation_id).cloned() else {
            return Ok(());
        };
        if !self.goal_controller.begin_blocking() {
            return Err(RuntimeHostError::GoalControlFailed {
                message: "another Goal Store request is still in flight".to_string(),
            });
        }
        let completion_tx = self.goal_controller.completion_sender();
        let message = message.to_string();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                prepare_goal_pause_worker(control, reason, message)
            })
            .await
            .map_err(|error| RuntimeHostError::GoalControlFailed {
                message: format!("goal pause worker failed: {error}"),
            })
            .and_then(|result| result);
            let _ = completion_tx
                .send(GoalBlockingCompletion::Pause {
                    operation_id,
                    result,
                })
                .await;
        });
        Ok(())
    }

    pub(super) fn commit_goal_surface_mutation_with_retry(
        &mut self,
        mutation: &GoalSurfaceMutationRecord,
    ) -> Result<Option<surface::SurfaceCommitBatch>, RuntimeHostError> {
        let thread_id = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .thread
            .thread_id
            .clone();
        let (goal_fence, receipt_digest, commit_id, scope, event) =
            surface_goal_mutation_event(mutation, thread_id)?;
        if self
            .resident_surface
            .coordinator
            .state()
            .has_goal_store_receipt(&commit_id, &receipt_digest)
        {
            return Ok(None);
        }
        let operation = goal_surface_operation(mutation);
        let mut events = vec![(scope, event)];
        if let Some(operation) = operation {
            events.push((
                surface::SurfaceScope::Operation {
                    operation_id: operation.operation_id.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::Requested {
                    operation: operation.clone(),
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, Some(commit_id));
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            let result = if operation.is_some() {
                self.resident_surface.coordinator.commit_actor_goal_batch(
                    goal_fence.clone(),
                    receipt_digest.clone(),
                    &batch,
                )
            } else {
                self.resident_surface.coordinator.commit_goal_batch(
                    goal_fence.clone(),
                    receipt_digest.clone(),
                    &batch,
                )
            };
            match result {
                Ok(_) => return Ok(Some(batch)),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(RuntimeHostError::GoalControlFailed {
                        message: format!("failed to commit typed Goal mutation: {error:?}"),
                    });
                }
            }
        }
        Err(RuntimeHostError::GoalControlFailed {
            message: "typed Goal mutation did not commit after bounded retries".to_string(),
        })
    }

    pub(super) fn commit_goal_edit_and_run_with_retry(
        &mut self,
        edited: &GoalSurfaceMutationRecord,
        started: &GoalSurfaceMutationRecord,
    ) -> Result<surface::SurfaceCommitBatch, RuntimeHostError> {
        let thread_id = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .thread
            .thread_id
            .clone();
        let (edited_fence, edited_digest, edited_commit_id, edited_scope, edited_event) =
            surface_goal_mutation_event(edited, thread_id.clone())?;
        let (started_fence, started_digest, _, started_scope, started_event) =
            surface_goal_mutation_event(started, thread_id)?;
        let operation =
            goal_surface_operation(started).ok_or_else(|| RuntimeHostError::GoalControlFailed {
                message: "Goal edit-and-run did not retain its requested operation".to_string(),
            })?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![
                (edited_scope, edited_event),
                (started_scope, started_event),
                (
                    surface::SurfaceScope::Operation {
                        operation_id: operation.operation_id.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Requested {
                        operation: operation.clone(),
                    }),
                ),
            ],
            Some(edited_commit_id),
        );
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self
                .resident_surface
                .coordinator
                .commit_actor_two_goal_batch(
                    edited_fence.clone(),
                    edited_digest.clone(),
                    started_fence.clone(),
                    started_digest.clone(),
                    &batch,
                ) {
                Ok(_) => return Ok(batch),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(RuntimeHostError::GoalControlFailed {
                        message: format!(
                            "failed to commit typed Goal edit-and-run mutation: {error:?}"
                        ),
                    });
                }
            }
        }
        Err(RuntimeHostError::GoalControlFailed {
            message: "typed Goal edit-and-run did not commit after bounded retries".to_string(),
        })
    }

    pub(super) fn settle_goal_surface_mutation(
        &mut self,
        runtime: &GoalRuntimeHandle,
        mutation: &GoalSurfaceMutationRecord,
    ) -> Result<(), RuntimeHostError> {
        self.settle_goal_surface_mutation_with_batch(runtime, mutation)
            .map(|_| ())
    }

    pub(super) fn settle_goal_surface_mutation_with_batch(
        &mut self,
        runtime: &GoalRuntimeHandle,
        mutation: &GoalSurfaceMutationRecord,
    ) -> Result<Option<surface::SurfaceCommitBatch>, RuntimeHostError> {
        let batch = self.commit_goal_surface_mutation_with_retry(mutation)?;
        Self::schedule_goal_surface_acknowledgement(runtime.clone(), mutation.clone());
        Ok(batch)
    }

    pub(super) fn schedule_goal_surface_acknowledgement(
        runtime: GoalRuntimeHandle,
        mutation: GoalSurfaceMutationRecord,
    ) {
        tokio::task::spawn_blocking(move || {
            Self::acknowledge_goal_surface_mutation_best_effort(&runtime, &mutation);
        });
    }

    pub(super) fn acknowledge_goal_surface_mutation_best_effort(
        runtime: &GoalRuntimeHandle,
        mutation: &GoalSurfaceMutationRecord,
    ) {
        match runtime.acknowledge_surface_mutation(
            &mutation.receipt.store_commit_id,
            &mutation.receipt.receipt_digest,
        ) {
            Ok(true) => {}
            Ok(false) => eprintln!(
                "orca: durable Goal outbox deferred exact acknowledgement for {}",
                mutation.receipt.store_commit_id
            ),
            Err(error) => eprintln!(
                "orca: durable Goal outbox acknowledgement will retry from recovery: {error}"
            ),
        }
    }

    pub(super) fn terminalize_surface_goal_completion_recovery(
        &mut self,
        active: &mut ActiveOperation,
        message: &str,
    ) -> Result<(), RuntimeHostError> {
        let operation_id = active
            .surface_operation
            .as_ref()
            .map(|fence| fence.operation_id.clone())
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed Goal recovery lost its operation".to_string(),
            })?;
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed Goal recovery operation is missing".to_string(),
            })?;
        if let Some(finalization) = operation.finalization.clone()
            && matches!(
                operation.phase,
                surface::OperationPhase::Finalizing { .. }
                    | surface::OperationPhase::FinalizingDegraded { .. }
            )
        {
            let usage = snapshot
                .usage
                .active_operation
                .as_ref()
                .filter(|(active_id, _)| active_id == &operation_id)
                .map(|(_, usage)| usage.clone())
                .unwrap_or(surface::UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                });
            let terminal = match &finalization.selected_cause {
                surface::OperationFinalizationCause::GenerationStop(reason) => match reason {
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::Success,
                    } => surface::OperationTerminal::Succeeded {
                        usage: usage.clone(),
                    },
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::VerificationFailed { message },
                    } => surface::OperationTerminal::Failed {
                        class: surface::FailureClass::Verification,
                        message: message.clone(),
                    },
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::BudgetExhausted { budget },
                    } => surface::OperationTerminal::BudgetExhausted {
                        budget: budget.clone(),
                    },
                    surface::GenerationStopReason::Cancelled { cause } => match cause {
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
                    },
                    surface::GenerationStopReason::ExecutionFailed { class, message } => {
                        let class = match class {
                            surface::GenerationExecutionFailureClass::Provider => {
                                surface::FailureClass::Provider
                            }
                            surface::GenerationExecutionFailureClass::Tool => {
                                surface::FailureClass::Tool
                            }
                            surface::GenerationExecutionFailureClass::Hook => {
                                surface::FailureClass::Hook
                            }
                            surface::GenerationExecutionFailureClass::Workflow => {
                                surface::FailureClass::Workflow
                            }
                            surface::GenerationExecutionFailureClass::InputResolution => {
                                surface::FailureClass::InputResolution
                            }
                            surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable => {
                                surface::FailureClass::ClientCapabilityUnavailable
                            }
                            surface::GenerationExecutionFailureClass::LegacyApprovalRequired => {
                                surface::FailureClass::LegacyApprovalRequired
                            }
                            surface::GenerationExecutionFailureClass::RuntimeInvariant => {
                                surface::FailureClass::RuntimeInvariant
                            }
                            surface::GenerationExecutionFailureClass::ExternalEffectAmbiguous => {
                                surface::FailureClass::ExternalEffectAmbiguous
                            }
                            surface::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous => {
                                surface::FailureClass::RemoteResourceCleanupAmbiguous
                            }
                        };
                        surface::OperationTerminal::Failed {
                            class,
                            message: message.clone(),
                        }
                    }
                    surface::GenerationStopReason::Panicked { message } => {
                        surface::OperationTerminal::Panicked {
                            message: message.clone(),
                        }
                    }
                    surface::GenerationStopReason::InterruptedResumable
                    | surface::GenerationStopReason::ProviderSuspended
                    | surface::GenerationStopReason::RuntimeRestart => {
                        surface::OperationTerminal::AbortedByRuntimeRestart {
                            last_generation: operation
                                .generations
                                .last()
                                .map(|generation| generation.fence.generation_id.clone())
                                .expect("finalizing operation has a generation"),
                        }
                    }
                    surface::GenerationStopReason::ProjectionFailure { message } => {
                        surface::OperationTerminal::Failed {
                            class: surface::FailureClass::Persistence,
                            message: message.clone(),
                        }
                    }
                    surface::GenerationStopReason::NotStarted { .. } => {
                        surface::OperationTerminal::Failed {
                            class: surface::FailureClass::RuntimeInvariant,
                            message: surface::SafeDiagnosticText::try_new(
                                "typed Goal completion failed before generation started",
                            )
                            .expect("static Goal recovery diagnostic is bounded"),
                        }
                    }
                },
                _ => surface::OperationTerminal::Failed {
                    class: surface::FailureClass::RuntimeInvariant,
                    message: surface::SafeDiagnosticText::try_new(
                        "typed Goal completion failed after durable finalization started",
                    )
                    .expect("static Goal recovery diagnostic is bounded"),
                },
            };
            let completion_proof =
                Self::surface_completion_proof(&snapshot, &operation, &terminal, None)?;
            let terminal_batch = self.surface_operation_batch_with_commit_id(
                &operation_id,
                vec![surface::OperationPatch::Terminal {
                    record: surface::OperationTerminalRecord {
                        operation_id: operation_id.clone(),
                        finalize_intent_id: finalization.finalize_intent_id.clone(),
                        terminal: terminal.clone(),
                        usage,
                        source_diagnostic_digest: None,
                        settlement_receipts: Vec::new(),
                        completion_proof: completion_proof.clone(),
                        committed_at: surface::UnixMillis::new(0),
                    },
                }],
                Some(finalization.terminal_commit_id.clone()),
            );
            let value = surface::OperationTerminalAtCursor {
                operation_id: operation_id.clone(),
                terminal,
                completion_proof,
                cursor: terminal_batch.cursor_after.clone(),
                commit_class: terminal_batch.commit_class.clone(),
                batch_digest: terminal_batch.batch_digest.clone(),
            };
            let legacy_terminal = OperationTerminal {
                operation_id: active.operation_id,
                outcome: OperationOutcome::ExecutionFailed {
                    kind: io::ErrorKind::Other,
                    message: message.to_string(),
                },
            };
            active.request.surface_goal_owned = false;
            if let Err(error) = self.resident_surface.coordinator.commit_finalizer_batch(
                operation_id.clone(),
                finalization.finalize_intent_id.clone(),
                &terminal_batch,
            ) {
                let repair = surface::RetryFinalizationToken::new(
                    operation.request_id,
                    snapshot.thread.thread_id,
                    operation_id.clone(),
                    finalization.finalize_intent_id.clone(),
                    finalization.terminal_commit_id.clone(),
                    snapshot.thread.owner_epoch,
                    terminal_batch.batch_digest.clone(),
                );
                self.cache_surface_terminal_failure(PendingSurfaceTerminalCommit {
                    batch: terminal_batch,
                    value,
                    failure: surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        operation_id,
                        finalize_intent_id: finalization.finalize_intent_id,
                        commit_id: finalization.terminal_commit_id,
                        repair,
                    },
                    legacy_completion: Some(active.completion.clone()),
                    legacy_terminal: Some(legacy_terminal),
                });
                eprintln!("orca: typed Goal recovery terminal commit failed: {error:?}");
                return Ok(());
            }
            self.cache_surface_terminal(value);
            self.goal_controller.clear_active(active.operation_id);
            let completed = active.completion.complete(legacy_terminal);
            debug_assert!(
                completed,
                "Goal recovery terminal must complete exactly once"
            );
            self.operation_recovery.terminal_blocked =
                Some("typed Goal terminal checkpoint failed before recovery completed".to_string());
            return Ok(());
        }
        let fence = operation
            .generations
            .iter()
            .rev()
            .find(|generation| generation.phase != surface::GenerationPhase::Stopped)
            .or_else(|| operation.generations.last())
            .map(|generation| generation.fence.clone())
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed Goal recovery operation has no generation".to_string(),
            })?;
        active.surface_operation = Some(fence.clone());
        active.request.surface_goal_owned = false;
        let outcome = OperationOutcome::ExecutionFailed {
            kind: io::ErrorKind::Other,
            message: message.to_string(),
        };
        let legacy_terminal = OperationTerminal {
            operation_id: active.operation_id,
            outcome,
        };
        let legacy = Some((active.completion.clone(), legacy_terminal));
        match self.repair_surface_admission_failure_with_legacy(
            &fence,
            "typed Goal durable completion recovery",
            legacy,
            true,
        ) {
            Ok(_) => {
                self.refresh_surface_goal_completion_recovery_block();
                Ok(())
            }
            Err(_)
                if self
                    .resident_surface
                    .commit
                    .has_admission_repair(&operation_id)
                    || self
                        .resident_surface
                        .commit
                        .has_admission_terminal(&operation_id) =>
            {
                Ok(())
            }
            Err(error) => Err(RuntimeHostError::ThreadStartFailed {
                message: format!("typed Goal recovery could not retain terminal repair: {error:?}"),
            }),
        }
    }

    pub(super) fn refresh_surface_goal_completion_recovery_block(&mut self) {
        self.operation_recovery.terminal_blocked =
            self.goal_controller.pending_recovery().map(|pending| {
                format!(
                    "typed Goal completion recovery is pending: {}",
                    pending.message
                )
            });
    }

    pub(super) fn retain_surface_goal_completion_recovery(
        &mut self,
        active: ActiveOperation,
        message: String,
    ) {
        self.operation_recovery.terminal_blocked = Some(format!(
            "typed Goal completion recovery is pending: {message}"
        ));
        let pending = PendingSurfaceGoalCompletionRecovery {
            active,
            message,
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        };
        self.goal_controller.enqueue_pending_recovery(pending);
    }

    pub(super) fn dispatch_surface_goal_completion_recovery(
        &mut self,
        active: ActiveOperation,
        message: String,
    ) {
        let retain_prepared_goal_batch = self
            .resident_surface
            .coordinator
            .incomplete_batch()
            .is_some_and(|batch| {
                batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        surface::SurfaceEvent::Goal(surface::GoalPatchEnvelope {
                            patch: surface::GoalPatch::OuterTurnFinished { .. },
                            ..
                        })
                    )
                })
            });
        if retain_prepared_goal_batch {
            self.retain_surface_goal_completion_recovery(
                active,
                format!(
                    "typed Goal recovery retained its exact prepared batch for cold recovery; {message}"
                ),
            );
            return;
        }
        if let Err(error) = self.resident_surface.coordinator.retry_incomplete_batch() {
            self.retain_surface_goal_completion_recovery(
                active,
                format!(
                    "typed Goal recovery could not reconcile its exact prepared batch: {error:?}; {message}"
                ),
            );
            return;
        }
        let control = match self
            .goal_controller
            .active_control(active.operation_id)
            .cloned()
        {
            Some(control) => control,
            None => {
                self.retain_surface_goal_completion_recovery(
                    active,
                    format!("typed Goal recovery lacks its runtime owner; {message}"),
                );
                return;
            }
        };
        if active.surface_operation.is_none() {
            self.retain_surface_goal_completion_recovery(
                active,
                format!("typed Goal recovery lost its operation; {message}"),
            );
            return;
        }
        if self.goal_controller.is_blocking() {
            self.retain_surface_goal_completion_recovery(active, message);
            return;
        }
        let spawned = self.spawn_goal_blocking(
            "typed Goal recovery outbox read",
            GoalBlockingCompletionKind::Recovery,
            move || read_goal_recovery_pending_worker(control),
            move |actor, result| {
                actor.continue_surface_goal_recovery_after_pending(active, message, result);
            },
        );
        debug_assert!(
            spawned.is_ok(),
            "Goal recovery outbox read was prevalidated"
        );
    }

    pub(super) fn continue_surface_goal_recovery_after_pending(
        &mut self,
        active: ActiveOperation,
        message: String,
        result: Result<GoalRecoveryPendingResult, RuntimeHostError>,
    ) {
        let GoalRecoveryPendingResult { control, pending } = match result {
            Ok(result) => result,
            Err(error) => {
                self.retain_surface_goal_completion_recovery(active, format!("{message}; {error}"));
                return;
            }
        };
        let mut unapplied_store_commit_ids = BTreeSet::new();
        for mutation in &pending {
            let (_, receipt_digest, commit_id, _, _) = match surface_goal_mutation_event(
                mutation,
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .thread
                    .thread_id
                    .clone(),
            ) {
                Ok(event) => event,
                Err(error) => {
                    self.retain_surface_goal_completion_recovery(
                        active,
                        format!("{message}; {error}"),
                    );
                    return;
                }
            };
            if !self
                .resident_surface
                .coordinator
                .state()
                .has_goal_store_receipt(&commit_id, &receipt_digest)
            {
                unapplied_store_commit_ids.insert(mutation.receipt.store_commit_id.clone());
            }
        }
        let work = InterruptedGoalRecoveryWork {
            control,
            snapshot: self.resident_surface.coordinator.state().snapshot().clone(),
            pending,
            unapplied_store_commit_ids,
        };
        if self.goal_controller.is_blocking() {
            self.retain_surface_goal_completion_recovery(active, message);
            return;
        }
        let spawned = self.spawn_goal_blocking(
            "typed Goal interrupted-continuation recovery",
            GoalBlockingCompletionKind::Recovery,
            move || prepare_interrupted_goal_recovery_worker(work),
            move |actor, result| {
                actor.continue_surface_goal_recovery_after_interrupted(active, message, result);
            },
        );
        debug_assert!(
            spawned.is_ok(),
            "Goal interrupted-continuation recovery was prevalidated"
        );
    }

    pub(super) fn continue_surface_goal_recovery_after_interrupted(
        &mut self,
        active: ActiveOperation,
        message: String,
        result: Result<InterruptedGoalRecoveryResult, RuntimeHostError>,
    ) {
        let InterruptedGoalRecoveryResult {
            control,
            pending,
            superseded,
            replacement,
        } = match result {
            Ok(result) => result,
            Err(error) => {
                self.retain_surface_goal_completion_recovery(active, format!("{message}; {error}"));
                return;
            }
        };
        if let Some(replacement) = replacement
            && let Err(error) = self.settle_goal_surface_mutation(&control.runtime, &replacement)
        {
            self.retain_surface_goal_completion_recovery(active, format!("{message}; {error}"));
            return;
        }
        for mutation in pending {
            if superseded.contains(&mutation.receipt.store_commit_id) {
                continue;
            }
            if let Err(error) = self.settle_goal_surface_mutation(&control.runtime, &mutation) {
                self.retain_surface_goal_completion_recovery(active, format!("{message}; {error}"));
                return;
            }
        }
        let operation_id = active
            .surface_operation
            .as_ref()
            .map(|fence| fence.operation_id.clone())
            .expect("Goal recovery operation was prevalidated");
        let work = GoalRunRecoveryWork {
            control,
            snapshot: self.resident_surface.coordinator.state().snapshot().clone(),
            operation_id,
            message: message.clone(),
        };
        if self.goal_controller.is_blocking() {
            self.retain_surface_goal_completion_recovery(active, message);
            return;
        }
        let spawned = self.spawn_goal_blocking(
            "typed Goal run recovery",
            GoalBlockingCompletionKind::Recovery,
            move || prepare_goal_run_recovery_worker(work),
            move |actor, result| {
                actor.finish_surface_goal_completion_recovery(active, message, result);
            },
        );
        debug_assert!(spawned.is_ok(), "Goal run recovery was prevalidated");
    }

    pub(super) fn finish_surface_goal_completion_recovery(
        &mut self,
        mut active: ActiveOperation,
        message: String,
        result: Result<GoalRunRecoveryResult, RuntimeHostError>,
    ) {
        let result = result.and_then(|prepared| {
            if let Some(mutation) = prepared.mutation {
                self.settle_goal_surface_mutation(&prepared.runtime, &mutation)?;
            }
            self.terminalize_surface_goal_completion_recovery(&mut active, &message)
        });
        if let Err(error) = result {
            self.retain_surface_goal_completion_recovery(active, format!("{message}; {error}"));
        }
    }

    pub(super) fn prepare_typed_goal_run_operation(
        &self,
        snapshot: &surface::SurfaceSnapshot,
        request_id: surface::SurfaceRequestId,
        goal_id: surface::SurfaceGoalId,
        goal_run_id: surface::SurfaceGoalRunId,
        objective_revision: surface::GoalObjectiveRevision,
        request: surface::SurfaceInputRequest,
    ) -> Result<
        (
            surface::OperationRecord,
            surface::SurfaceOperationId,
            surface::SurfaceAdmissionLeaseId,
        ),
        surface::SurfaceClientCommandError,
    > {
        if resolve_surface_input(&request).is_none() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
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
        let admission_lease_id = lease.lease_id.clone();
        let request_digest = surface_sha256(
            &serde_json::to_vec(&request).expect("Goal input request is serializable"),
        );
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id,
            intent: surface::OperationIntent {
                origin: surface::OperationOrigin::TuiUser,
                kind: surface::OperationKind::GoalRun {
                    goal_id,
                    goal_run_id,
                    initial_objective_revision: objective_revision,
                },
                initial_replayability: surface::Replayability::Replayable {
                    capsule_digest: request_digest.clone(),
                    request: Some(request),
                    request_digest: Some(request_digest),
                    cwd: snapshot.settings.effective.cwd.clone(),
                    workspace_roots: snapshot.settings.effective.workspace_roots.clone(),
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                    tool_schema_digest: surface_sha256(
                        &serde_json::to_vec(&snapshot.tools)
                            .expect("surface tools are serializable"),
                    ),
                },
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: crate::runtime_host::surface_capability_fingerprint(
                    &snapshot.settings.effective,
                    &snapshot.tools,
                ),
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
        Ok((operation, operation_id, admission_lease_id))
    }

    pub(super) fn dispatch_goal_mutation_command(
        &mut self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::GoalMutationAction,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let Some(runtime) = self
            .state
            .as_ref()
            .and_then(|state| state.thread.initialized_goal_runtime_handle())
        else {
            self.goal_controller
                .defer(ThreadCommand::SurfaceGoalMutation {
                    client,
                    request_id,
                    action,
                    reply,
                });
            let (open_reply, _receive) = mpsc::sync_channel(1);
            self.open_goal_runtime_off_actor(open_reply);
            return;
        };
        match action {
            surface::GoalMutationAction::Edit {
                fence,
                objective,
                token_budget,
            } => self.dispatch_typed_goal_edit(
                runtime,
                request_id,
                fence,
                objective,
                token_budget,
                reply,
            ),
            surface::GoalMutationAction::Clear { fence } => {
                self.dispatch_typed_goal_clear(runtime, request_id, fence, reply);
            }
            surface::GoalMutationAction::SetAndRun {
                expected_goal,
                objective,
                token_budget,
                input,
            } => match expected_goal {
                surface::ExpectedGoal::Exact(fence) => self.dispatch_typed_goal_replace_and_run(
                    runtime,
                    client,
                    request_id,
                    fence,
                    objective,
                    token_budget,
                    input,
                    reply,
                ),
                _ => self.dispatch_typed_goal_create_and_run(
                    runtime,
                    client,
                    request_id,
                    objective,
                    token_budget,
                    input,
                    reply,
                ),
            },
            surface::GoalMutationAction::ResumeAndRun { fence, input } => {
                self.dispatch_typed_goal_resume_and_run(
                    runtime, client, request_id, fence, input, reply,
                );
            }
        }
    }

    pub(super) fn dispatch_typed_goal_create_and_run(
        &mut self,
        runtime: GoalRuntimeHandle,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        objective: surface::NonEmptyText,
        token_budget: Option<i64>,
        input: surface::GoalRunInput,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let surface::GoalRunInput::Supplied { request } = input else {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            };
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            if snapshot.goal.is_some() {
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            let goal_id = orca_core::goal_runtime::GoalId::new();
            let goal_run_id = orca_core::goal_runtime::GoalRunId::new();
            let surface_goal_id = surface::SurfaceGoalId::try_new(goal_id.to_string())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let surface_goal_run_id =
                surface::SurfaceGoalRunId::try_new(goal_run_id.to_string())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let (operation, operation_id, admission_lease_id) = self
                .prepare_typed_goal_run_operation(
                    &snapshot,
                    request_id.clone(),
                    surface_goal_id,
                    surface_goal_run_id,
                    surface::GoalObjectiveRevision::new(1),
                    request.clone(),
                )?;
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&(
                    "goal_set_and_run",
                    request_id.as_bytes(),
                    objective.as_str(),
                    token_budget,
                    &request,
                    operation_id.as_bytes(),
                ))
                .expect("Goal set-and-run digest input is serializable"),
            )
            .as_bytes();
            Ok((
                session_id.clone(),
                TypedGoalSurfaceWork::CreateAndRun {
                    input: CreateGoalAndPrepareRunForSurfaceInput {
                        goal: CreateGoalInput {
                            session_id,
                            objective: objective.as_str().to_string(),
                            token_budget,
                            now: chrono::Utc::now().timestamp(),
                        },
                        goal_id,
                        goal_run_id,
                        operation: Box::new(operation),
                        origin: orca_core::goal_runtime::GoalTurnOrigin::User,
                    },
                    context: GoalSurfaceMutationContext {
                        store_commit_id: uuid::Uuid::now_v7().to_string(),
                        command_digest,
                        goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                    },
                },
                operation_id,
                admission_lease_id,
            ))
        })();
        let (session_id, work, operation_id, admission_lease_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            "typed Goal create-and-run",
            GoalBlockingCompletionKind::SurfaceMutation,
            move || prepare_typed_goal_surface_worker(runtime, session_id, work),
            move |actor, result| match result {
                Ok(worker) => actor.dispatch_typed_goal_run_preparation(
                    client,
                    request_id,
                    operation_id,
                    admission_lease_id,
                    worker,
                    reply,
                ),
                Err(_) => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn dispatch_typed_goal_replace_and_run(
        &mut self,
        runtime: GoalRuntimeHandle,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceGoalFence,
        objective: surface::NonEmptyText,
        token_budget: Option<i64>,
        input: surface::GoalRunInput,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let goal = snapshot
                .goal
                .as_ref()
                .filter(|goal| {
                    goal.goal_id == fence.goal_id
                        && goal.goal_revision == fence.goal_revision
                        && goal.goal_owner_epoch == fence.goal_owner_epoch
                        && goal.current_run.is_none()
                        && !matches!(goal.state, surface::SurfaceGoalState::Complete { .. })
                })
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let request = match input {
                surface::GoalRunInput::Supplied { request } => request,
                surface::GoalRunInput::DerivedFromGoal {
                    goal_id,
                    objective_revision,
                    goal_receipt_digest,
                } => {
                    if goal_id != goal.goal_id
                        || objective_revision != goal.objective_revision
                        || goal_receipt_digest != goal.receipt_digest
                    {
                        return Err(surface::SurfaceClientCommandError::Unauthorized);
                    }
                    surface::SurfaceInputRequest {
                        blocks: surface::NonEmptyVec::try_new(vec![
                            surface::SurfaceInputRequestBlock::Text {
                                text: surface::DisplayText::new(objective.as_str()),
                            },
                        ])
                        .expect("Goal objective produces one input block"),
                    }
                }
            };
            let expected_receipt_digest = *goal.receipt_digest.as_bytes();
            let goal_id = orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_run_id = orca_core::goal_runtime::GoalRunId::new();
            let surface_goal_run_id =
                surface::SurfaceGoalRunId::try_new(goal_run_id.to_string())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let objective_revision = if goal.objective.as_str() == objective.as_str() {
                goal.objective_revision
            } else {
                surface::GoalObjectiveRevision::new(
                    goal.objective_revision
                        .get()
                        .checked_add(1)
                        .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                )
            };
            let (operation, operation_id, admission_lease_id) = self
                .prepare_typed_goal_run_operation(
                    &snapshot,
                    request_id.clone(),
                    goal.goal_id,
                    surface_goal_run_id,
                    objective_revision,
                    request.clone(),
                )?;
            let expected_goal_revision = u32::try_from(fence.goal_revision.get())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let common_digest = (
                "goal_replace_and_run",
                request_id.as_bytes(),
                &fence,
                objective.as_str(),
                token_budget,
                &request,
                operation_id.as_bytes(),
            );
            let contexts = [
                GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest: *surface_sha256(
                        &serde_json::to_vec(&(&common_digest, "edit"))
                            .expect("Goal replacement edit digest input is serializable"),
                    )
                    .as_bytes(),
                    goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                },
                GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest: *surface_sha256(
                        &serde_json::to_vec(&(&common_digest, "run"))
                            .expect("Goal replacement run digest input is serializable"),
                    )
                    .as_bytes(),
                    goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                },
            ];
            Ok((
                session_id.clone(),
                TypedGoalSurfaceWork::EditAndRun {
                    input: EditGoalAndPrepareRunForSurfaceInput {
                        session_id,
                        expected_goal_id: goal_id,
                        expected_goal_revision,
                        expected_receipt_digest,
                        objective: objective.as_str().to_string(),
                        token_budget,
                        goal_run_id,
                        operation: Box::new(operation),
                        origin: orca_core::goal_runtime::GoalTurnOrigin::User,
                        started_at: chrono::Utc::now().timestamp(),
                    },
                    contexts,
                },
                operation_id,
                admission_lease_id,
            ))
        })();
        let (session_id, work, operation_id, admission_lease_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            "typed Goal replace-and-run",
            GoalBlockingCompletionKind::SurfaceMutation,
            move || prepare_typed_goal_surface_worker(runtime, session_id, work),
            move |actor, result| match result {
                Ok(worker) => actor.dispatch_typed_goal_run_preparation(
                    client,
                    request_id,
                    operation_id,
                    admission_lease_id,
                    worker,
                    reply,
                ),
                Err(_) => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn dispatch_typed_goal_resume_and_run(
        &mut self,
        runtime: GoalRuntimeHandle,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceGoalFence,
        input: surface::GoalRunInput,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let goal = snapshot
                .goal
                .as_ref()
                .filter(|goal| {
                    goal.goal_id == fence.goal_id
                        && goal.goal_revision == fence.goal_revision
                        && goal.goal_owner_epoch == fence.goal_owner_epoch
                        && goal.current_run.is_none()
                        && !matches!(goal.state, surface::SurfaceGoalState::Complete { .. })
                })
                .cloned()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let request = match input {
                surface::GoalRunInput::Supplied { request } => request,
                surface::GoalRunInput::DerivedFromGoal {
                    goal_id,
                    objective_revision,
                    goal_receipt_digest,
                } => {
                    if goal_id != goal.goal_id
                        || objective_revision != goal.objective_revision
                        || goal_receipt_digest != goal.receipt_digest
                    {
                        return Err(surface::SurfaceClientCommandError::Unauthorized);
                    }
                    surface::SurfaceInputRequest {
                        blocks: surface::NonEmptyVec::try_new(vec![
                            surface::SurfaceInputRequestBlock::Text {
                                text: surface::DisplayText::new(goal.objective.as_str()),
                            },
                        ])
                        .expect("Goal objective produces one input block"),
                    }
                }
            };
            let expected_receipt_digest = *goal.receipt_digest.as_bytes();
            let goal_id = orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_run_id = orca_core::goal_runtime::GoalRunId::new();
            let surface_goal_run_id =
                surface::SurfaceGoalRunId::try_new(goal_run_id.to_string())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let (operation, operation_id, admission_lease_id) = self
                .prepare_typed_goal_run_operation(
                    &snapshot,
                    request_id.clone(),
                    goal.goal_id,
                    surface_goal_run_id,
                    goal.objective_revision,
                    request.clone(),
                )?;
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&(
                    "goal_resume_and_run",
                    request_id.as_bytes(),
                    &fence,
                    &request,
                    operation_id.as_bytes(),
                ))
                .expect("Goal resume digest input is serializable"),
            )
            .as_bytes();
            Ok((
                session_id.clone(),
                TypedGoalSurfaceWork::PrepareRun {
                    input: PrepareGoalRunForSurfaceInput {
                        session_id,
                        expected_goal_id: goal_id,
                        expected_goal_revision: u32::try_from(fence.goal_revision.get())
                            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                        expected_receipt_digest,
                        goal_run_id,
                        operation: Box::new(operation),
                        origin: orca_core::goal_runtime::GoalTurnOrigin::Resume,
                        started_at: chrono::Utc::now().timestamp(),
                    },
                    context: GoalSurfaceMutationContext {
                        store_commit_id: uuid::Uuid::now_v7().to_string(),
                        command_digest,
                        goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                    },
                },
                operation_id,
                admission_lease_id,
            ))
        })();
        let (session_id, work, operation_id, admission_lease_id) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            "typed Goal resume-and-run",
            GoalBlockingCompletionKind::SurfaceMutation,
            move || prepare_typed_goal_surface_worker(runtime, session_id, work),
            move |actor, result| match result {
                Ok(worker) => actor.dispatch_typed_goal_run_preparation(
                    client,
                    request_id,
                    operation_id,
                    admission_lease_id,
                    worker,
                    reply,
                ),
                Err(_) => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn dispatch_typed_goal_edit(
        &mut self,
        runtime: GoalRuntimeHandle,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceGoalFence,
        objective: surface::NonEmptyText,
        token_budget: surface::GoalTokenBudgetUpdate,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let goal = snapshot
                .goal
                .as_ref()
                .filter(|goal| {
                    goal.goal_id == fence.goal_id
                        && goal.goal_revision == fence.goal_revision
                        && goal.goal_owner_epoch == fence.goal_owner_epoch
                        && goal.current_run.is_none()
                })
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_id = orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let expected_revision = u32::try_from(fence.goal_revision.get())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let budget_update = match token_budget {
                surface::GoalTokenBudgetUpdate::Keep => GoalSurfaceTokenBudgetUpdate::Keep,
                surface::GoalTokenBudgetUpdate::Set(budget) => {
                    GoalSurfaceTokenBudgetUpdate::Set(budget)
                }
            };
            let budget_digest = match token_budget {
                surface::GoalTokenBudgetUpdate::Keep => (false, None),
                surface::GoalTokenBudgetUpdate::Set(budget) => (true, budget),
            };
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&(
                    "goal_edit",
                    request_id.as_bytes(),
                    &fence,
                    objective.as_str(),
                    budget_digest,
                ))
                .expect("Goal edit digest input is serializable"),
            )
            .as_bytes();
            Ok((
                session_id.clone(),
                TypedGoalSurfaceWork::Edit {
                    session_id,
                    goal_id,
                    expected_revision,
                    objective: objective.as_str().to_string(),
                    token_budget: budget_update,
                    at: chrono::Utc::now().timestamp(),
                    context: GoalSurfaceMutationContext {
                        store_commit_id: uuid::Uuid::now_v7().to_string(),
                        command_digest,
                        goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                    },
                },
            ))
        })();
        let (session_id, work) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        self.dispatch_typed_goal_single_mutation(
            runtime,
            session_id,
            work,
            "typed Goal edit",
            request_id,
            reply,
        );
    }

    pub(super) fn dispatch_typed_goal_clear(
        &mut self,
        runtime: GoalRuntimeHandle,
        request_id: surface::SurfaceRequestId,
        fence: surface::SurfaceGoalFence,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let prepared = (|| {
            let session_id = self
                .handle
                .session_id
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
            let goal = snapshot
                .goal
                .as_ref()
                .filter(|goal| {
                    goal.goal_id == fence.goal_id
                        && goal.goal_revision == fence.goal_revision
                        && goal.goal_owner_epoch == fence.goal_owner_epoch
                        && goal.current_run.is_none()
                })
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let goal_id = orca_core::goal_runtime::GoalId::parse(goal.goal_id.as_str())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let expected_revision = u32::try_from(fence.goal_revision.get())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let command_digest = *surface_sha256(
                &serde_json::to_vec(&("goal_clear", request_id.as_bytes(), &fence))
                    .expect("Goal clear digest input is serializable"),
            )
            .as_bytes();
            Ok((
                session_id.clone(),
                TypedGoalSurfaceWork::Clear {
                    session_id,
                    goal_id,
                    expected_revision,
                    context: GoalSurfaceMutationContext {
                        store_commit_id: uuid::Uuid::now_v7().to_string(),
                        command_digest,
                        goal_owner_epoch: snapshot.thread.owner_epoch.get(),
                    },
                },
            ))
        })();
        let (session_id, work) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        self.dispatch_typed_goal_single_mutation(
            runtime,
            session_id,
            work,
            "typed Goal clear",
            request_id,
            reply,
        );
    }

    pub(super) fn dispatch_typed_goal_single_mutation(
        &mut self,
        runtime: GoalRuntimeHandle,
        session_id: String,
        work: TypedGoalSurfaceWork,
        operation: &'static str,
        request_id: surface::SurfaceRequestId,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::GoalMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    ) {
        let failure_reply = reply.clone();
        let spawned = self.spawn_goal_blocking(
            operation,
            GoalBlockingCompletionKind::SurfaceMutation,
            move || prepare_typed_goal_surface_worker(runtime, session_id, work),
            move |actor, result| {
                let result = result
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)
                    .and_then(|worker| {
                        let (primary, batch) = actor
                            .settle_typed_goal_surface_worker(worker)
                            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
                        let [mutation] = primary.as_slice() else {
                            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                        };
                        actor.goal_mutation_reply(request_id, mutation, &batch, None, None)
                    });
                let _ = reply.send(result);
            },
        );
        if spawned.is_err() {
            let _ = failure_reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
        }
    }

    pub(super) fn goal_mutation_reply(
        &self,
        request_id: surface::SurfaceRequestId,
        mutation: &GoalSurfaceMutationRecord,
        batch: &surface::SurfaceCommitBatch,
        operation_id: Option<surface::SurfaceOperationId>,
        waiter: Option<surface::OperationWaiterHandle>,
    ) -> Result<
        surface::MutationReply<surface::GoalMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        let (_, _, _, _, event) =
            surface_goal_mutation_event(mutation, batch.cursor_after.thread_id.clone())
                .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let surface::SurfaceEvent::Goal(goal_event) = event else {
            unreachable!("Goal mutation projects a Goal event")
        };
        let acknowledgements = batch
            .events
            .as_slice()
            .iter()
            .filter_map(|event| {
                let family = match &event.event {
                    surface::SurfaceEvent::Goal(_) => surface::SurfaceFactFamily::Goal,
                    surface::SurfaceEvent::Operation(_) => surface::SurfaceFactFamily::Operation,
                    _ => return None,
                };
                Some(surface::MutationCommitAck::ThreadLocalCursor {
                    cursor: batch.cursor_after.clone(),
                    family,
                    event_id: event.event_id.clone(),
                    commit_class: batch.commit_class.clone(),
                })
            })
            .collect::<Vec<_>>();
        Ok(surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Goal {
                    goal_id: goal_event.receipt.goal_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(acknowledgements)
                    .expect("Goal mutation has one acknowledgement"),
            },
            value: surface::GoalMutationOutput {
                goal: self
                    .resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .goal
                    .clone(),
                goal_receipt: goal_event.receipt,
                change_cursor: batch.cursor_after.clone(),
                operation_id,
                waiter,
            },
        })
    }
}
