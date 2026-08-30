// Mechanical ThreadActor method boundary; state ownership lives in runtime_actor controllers.
use super::*;

impl ThreadActor {
    pub(super) fn launch_hosted_workflow(
        &mut self,
        request: HostedWorkflowRequest,
    ) -> Result<HostedWorkflowLaunch, RuntimeHostError> {
        self.ensure_background_capacity(1).map_err(|error| {
            RuntimeHostError::WorkflowLaunchFailed {
                message: error.to_string(),
            }
        })?;
        let Some(mut state) = self.state.take() else {
            return Err(RuntimeHostError::ThreadUnavailable);
        };
        let result = self.launch_hosted_workflow_with_state(&mut state, request);
        self.state = Some(state);
        result
    }

    pub(super) fn launch_hosted_workflow_with_state(
        &mut self,
        state: &mut ThreadActorState,
        request: HostedWorkflowRequest,
    ) -> Result<HostedWorkflowLaunch, RuntimeHostError> {
        let HostedWorkflowRequest {
            name,
            args,
            config,
            tool_use_id,
            event_observer,
        } = request;
        let tool_use_id =
            tool_use_id.unwrap_or_else(|| format!("workflow-{}", uuid::Uuid::new_v4()));
        let tool_request = orca_core::tool_types::ToolRequest {
            id: tool_use_id.clone(),
            name: orca_core::tool_types::ToolName::Workflow,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some(name.clone()),
            raw_arguments: serde_json::to_string(&WorkflowInput {
                name: Some(name.clone()),
                args: args.clone(),
                ..Default::default()
            })
            .ok(),
        };
        observe_runtime_event(
            event_observer.as_deref(),
            state.events.tool_call_requested(&tool_request),
        );

        let config = config.unwrap_or_else(|| self.config.clone());
        if !config.workflows.enabled {
            let message = "workflows are disabled".to_string();
            let failed =
                orca_core::tool_types::ToolResult::failed(&tool_request, message.clone(), None);
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.tool_call_completed(&failed),
            );
            return Err(RuntimeHostError::WorkflowLaunchFailed { message });
        }
        let cwd = config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir().map_err(|error| {
                RuntimeHostError::WorkflowLaunchFailed {
                    message: error.to_string(),
                }
            })?);
        let task_registry = state.thread.session().task_registry().clone();
        let session_dir = task_registry.workflow_session_dir(&cwd).map_err(|error| {
            RuntimeHostError::WorkflowLaunchFailed {
                message: format!("failed to resolve workflow runtime storage: {error}"),
            }
        })?;
        let runner = WorkflowRunner::new(config, task_registry.clone(), session_dir);
        let launch = match runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
            name: Some(name),
            args,
            ..Default::default()
        })) {
            Ok(launch) => launch,
            Err(error) => {
                let message = error.to_string();
                let failed =
                    orca_core::tool_types::ToolResult::failed(&tool_request, message.clone(), None);
                observe_runtime_event(
                    event_observer.as_deref(),
                    state.events.tool_call_completed(&failed),
                );
                return Err(RuntimeHostError::WorkflowLaunchFailed { message });
            }
        };
        let response = HostedWorkflowLaunch {
            task_id: launch.task_id.clone(),
            run_id: launch.run_id.clone(),
            workflow_name: launch.workflow_name.clone(),
            tool_use_id: tool_use_id.clone(),
            output: launch.output.clone(),
        };
        observe_runtime_event(
            event_observer.as_deref(),
            state.events.workflow_started(
                &launch.task_id,
                &launch.run_id,
                &launch.workflow_name,
                &launch.phases,
            ),
        );
        if let Some(task) = task_registry
            .list()
            .into_iter()
            .find(|task| task.id == launch.task_id)
        {
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.task_status_updated(&task),
            );
        }
        if let Ok(output) = serde_json::to_string(&launch.output) {
            let completed =
                orca_core::tool_types::ToolResult::completed(&tool_request, output, false);
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.tool_call_completed(&completed),
            );
        }

        self.spawn_workflow_background_tasks(
            task_registry,
            &state.events,
            event_observer,
            RuntimeBackgroundWorkflows::from_vec(vec![BackgroundWorkflowRun::new(
                launch,
                Some(tool_use_id),
            )]),
        );
        Ok(response)
    }

    pub(super) fn ensure_background_capacity(&self, additional: usize) -> io::Result<()> {
        self.background_controller
            .ensure_capacity(additional)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "runtime host background task capacity exhausted ({})",
                        error.capacity()
                    ),
                )
            })
    }

    pub(super) fn spawn_workflow_background_tasks(
        &mut self,
        task_registry: TaskRegistry,
        events: &EventFactory,
        observer: Option<Arc<dyn EventObserver>>,
        workflows: RuntimeBackgroundWorkflows,
    ) {
        for workflow in workflows.into_inner() {
            let task_id = workflow.task_id.clone();
            let completion_task_id = task_id.clone();
            let completion_tx = self.background_controller.completion_notifier();
            let cancel = CancelToken::new();
            let worker_cancel = cancel.clone();
            let context = WorkflowBackgroundTaskContext {
                task_registry: task_registry.clone(),
                observer: observer.clone(),
                events: events.fork(),
            };
            let join = tokio::task::spawn_blocking(move || {
                let panic_registry = context.task_registry.clone();
                let panic_observer = context.observer.clone();
                let mut panic_events = context.events.fork();
                let panic_task_id = workflow.task_id.clone();
                let panic_run_id = workflow.run_id.clone();
                let panic_workflow_name = workflow.workflow_name.clone();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_workflow_background_task(workflow, context, &worker_cancel)
                }));
                if let Err(payload) = outcome {
                    let message = panic_message(payload);
                    let _ = panic_registry.fail(&panic_task_id, message.clone());
                    emit_workflow_task_status(
                        panic_observer.as_deref(),
                        &mut panic_events,
                        &panic_registry,
                        &panic_task_id,
                    );
                    observe_runtime_event(
                        panic_observer.as_deref(),
                        panic_events.workflow_failed(
                            &panic_task_id,
                            &panic_run_id,
                            &panic_workflow_name,
                            None,
                            &message,
                        ),
                    );
                }
                let _ = completion_tx.send(completion_task_id);
            });
            self.background_controller
                .admit_task(
                    task_id,
                    HostBackgroundTask {
                        cancel,
                        join,
                        typed_workflow: None,
                        typed_provider: None,
                    },
                )
                .expect("background capacity was checked before workflow admission");
        }
    }

    pub(super) fn spawn_provider_background_task(
        &mut self,
        active: &ActiveOperation,
        state: &mut ThreadActorState,
        suspension: Box<RuntimeProviderSuspension>,
        typed_provider: Option<TypedProviderBackground>,
    ) -> io::Result<String> {
        let task_id = active
            .main_session_task_id
            .clone()
            .ok_or_else(|| io::Error::other("provider suspension requires a main-session task"))?;
        self.ensure_background_capacity(1)?;

        let task_registry = state.thread.session().task_registry().clone();
        if typed_provider.is_none() {
            task_registry
                .mark_backgrounded(&task_id)
                .map_err(io::Error::other)?;
            emit_task_status_update(
                active.request.event_observer(),
                &mut state.events,
                &task_registry,
                &task_id,
            )?;
        } else {
            let _ = emit_task_status_update(
                active.request.event_observer(),
                &mut state.events,
                &task_registry,
                &task_id,
            );
        }

        let history_writer = state.thread.session_mut().writer_mut().cloned();
        let surface_outcome = typed_provider
            .as_ref()
            .map(|typed| Arc::clone(&typed.outcome));
        let surface_provider_ingress = typed_provider.as_ref().map(|typed| {
            Arc::new(RuntimeSurfaceProviderResponseIngress {
                command_tx: self.handle.command_tx.clone(),
                fence: typed.fence.operation_fence.clone(),
            }) as Arc<dyn surface::RuntimeProviderResponseIngress>
        });
        let context = ProviderBackgroundTaskContext {
            task_registry,
            history_writer,
            observer: active.request.event_observer(),
            events: state.events.fork(),
            model: suspension.model().map(str::to_string),
            task_id: task_id.clone(),
            usage_ledger: self.usage_ledger.clone(),
            response_identity: suspension.identity().clone(),
            surface_outcome,
            surface_provider_ingress,
        };
        #[cfg(test)]
        let completion_notify_delay = context
            .task_registry
            .get(&task_id)
            .and_then(|task| {
                task.description.split_whitespace().find_map(|token| {
                    token
                        .strip_prefix("test_provider_completion_notify_delay_ms=")
                        .and_then(|value| value.parse::<u64>().ok())
                })
            })
            .map(Duration::from_millis);
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let completion_tx = self.background_controller.completion_notifier();
        let completion_task_id = task_id.clone();
        let panic_surface_outcome = typed_provider
            .as_ref()
            .map(|typed| Arc::clone(&typed.outcome));
        let join = tokio::task::spawn_blocking(move || {
            let panic_registry = context.task_registry.clone();
            let panic_task_id = context.task_id.clone();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_provider_background_task(*suspension, context, &worker_cancel)
            }));
            if let Err(payload) = outcome {
                let message = panic_message(payload);
                if let Some(surface_outcome) = panic_surface_outcome {
                    let completed_at_ms = chrono::Utc::now().timestamp_millis();
                    let outcome = TypedProviderBackgroundOutcome {
                        response: None,
                        status: RunStatus::Failed,
                        error: Some(message),
                        usage: None,
                        operation_terminal: None,
                        completed_at_ms,
                    };
                    *surface_outcome
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome.clone());
                    let _ = panic_registry.record_typed_provider_outcome(
                        &panic_task_id,
                        durable_typed_provider_outcome(&outcome),
                    );
                } else {
                    let _ = panic_registry.apply_main_session_terminal_update(
                        &panic_task_id,
                        MainSessionTerminalUpdate::Failed { error: message },
                        None,
                    );
                }
            }
            #[cfg(test)]
            if let Some(delay) = completion_notify_delay {
                std::thread::sleep(delay);
            }
            let _ = completion_tx.send(completion_task_id);
        });
        self.background_controller
            .admit_task(
                task_id.clone(),
                HostBackgroundTask {
                    cancel,
                    join,
                    typed_workflow: None,
                    typed_provider,
                },
            )
            .map_err(|error| {
                io::Error::other(format!("background task admission failed: {error:?}"))
            })?;
        Ok(task_id)
    }

    pub(super) async fn reap_background_task(&mut self, task_id: &str) {
        if let Some(task) = self.background_controller.begin_completion(task_id) {
            let HostBackgroundTask {
                join,
                typed_workflow,
                typed_provider,
                ..
            } = task;
            let _ = join.await;
            if let Some(typed_workflow) = typed_workflow
                && let Err(error) = self.commit_typed_workflow_completion(typed_workflow, None)
            {
                self.operation_recovery.terminal_blocked = Some(error.to_string());
            }
            if let Some(typed_provider) = typed_provider
                && let Err(error) = self.commit_typed_provider_completion(typed_provider, None)
            {
                self.operation_recovery.terminal_blocked = Some(error.to_string());
            }
        }
    }

    pub(super) fn commit_typed_provider_completion(
        &mut self,
        typed: TypedProviderBackground,
        shutdown_reason: Option<surface::SurfaceShutdownReason>,
    ) -> Result<(), RuntimeHostError> {
        let operation_id = typed.fence.operation_fence.operation_id.clone();
        let mut pending = match self
            .prepare_typed_provider_completion(typed.clone(), shutdown_reason.clone())
        {
            Ok(pending) => pending,
            Err(error) => {
                eprintln!(
                    "orca: typed provider completion preparation deferred for {operation_id:?}: {error}"
                );
                self.background_controller.retain_provider_preparation(
                    operation_id,
                    PendingTypedProviderPreparation {
                        typed,
                        shutdown_reason,
                        retry_at: tokio::time::Instant::now()
                            + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                    },
                );
                return Err(error);
            }
        };
        let operation_id = pending.operation_id.clone();
        match self.settle_typed_provider_completion(&mut pending) {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!(
                    "orca: typed provider completion settlement deferred for {operation_id:?}: {error}"
                );
                pending.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                self.background_controller
                    .retain_provider_completion(operation_id, pending);
                Err(error)
            }
        }
    }

    pub(super) fn prepare_typed_provider_completion(
        &mut self,
        typed: TypedProviderBackground,
        shutdown_reason: Option<surface::SurfaceShutdownReason>,
    ) -> Result<PendingTypedProviderCompletion, RuntimeHostError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == typed.task_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed provider task disappeared before completion".to_string(),
            })?;
        if typed.task_registry.get(typed.task_id.as_str()).is_none() {
            return Err(RuntimeHostError::ThreadStartFailed {
                message: "typed provider task registry record disappeared".to_string(),
            });
        }
        let outcome = typed
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed provider task outcome disappeared before completion".to_string(),
            })?;
        if typed
            .task_registry
            .typed_provider_outcome(typed.task_id.as_str())
            .is_none()
        {
            typed
                .task_registry
                .record_typed_provider_outcome(
                    typed.task_id.as_str(),
                    durable_typed_provider_outcome(&outcome),
                )
                .map_err(|error| RuntimeHostError::ThreadStartFailed {
                    message: format!(
                        "failed to persist typed provider outcome before completion: {error}"
                    ),
                })?;
        }
        let committed_user_cancel = snapshot
            .operation_history
            .iter()
            .find(|operation| operation.operation_id == typed.fence.operation_fence.operation_id)
            .is_some_and(|operation| {
                matches!(
                    &operation.pending_control,
                    Some(surface::PendingControlIntent::Terminalize {
                        operation_id,
                        cause: surface::TerminalizationCause::UserCancel,
                    }) if operation_id == &typed.fence.operation_fence.operation_id
                )
            });
        let next_task_revision =
            surface::TaskRevision::try_new(task.revision.get().checked_add(1).ok_or_else(
                || RuntimeHostError::ThreadStartFailed {
                    message: "typed provider task revision exhausted".to_string(),
                },
            )?)
            .map_err(|_| RuntimeHostError::ThreadStartFailed {
                message: "typed provider task revision is invalid".to_string(),
            })?;
        let usage = surface_usage_totals(outcome.usage.unwrap_or_default());
        let failure_message = surface::SafeDiagnosticText::try_new("background provider failed")
            .expect("fixed diagnostic is bounded");
        let approval_message =
            surface::SafeDiagnosticText::try_new("background provider requires approval")
                .expect("fixed diagnostic is bounded");
        let approval_required = shutdown_reason.is_none()
            && !committed_user_cancel
            && outcome.status == RunStatus::ApprovalRequired;
        let budget_terminal = outcome
            .operation_terminal
            .as_ref()
            .and_then(surface_budget_from_core_terminal);
        let (task_status, stop_reason, terminal) =
            match (shutdown_reason.clone(), outcome.status, budget_terminal) {
                (Some(reason), _, _) => (
                    surface::SurfaceTaskStatus::Stopped,
                    surface::GenerationStopReason::Cancelled {
                        cause: match reason {
                            surface::SurfaceShutdownReason::HostShutdown => {
                                surface::TerminalizationCause::HostShutdown
                            }
                            surface::SurfaceShutdownReason::ThreadClose => {
                                surface::TerminalizationCause::ThreadClose
                            }
                        },
                    },
                    surface::OperationTerminal::Shutdown { reason },
                ),
                (None, _, _) if committed_user_cancel => (
                    surface::SurfaceTaskStatus::Cancelled,
                    surface::GenerationStopReason::Cancelled {
                        cause: surface::TerminalizationCause::UserCancel,
                    },
                    surface::OperationTerminal::Cancelled {
                        reason: surface::CancelReason::User,
                    },
                ),
                (None, _, Some(budget)) => (
                    surface::SurfaceTaskStatus::Stopped,
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::BudgetExhausted {
                            budget: budget.clone(),
                        },
                    },
                    surface::OperationTerminal::BudgetExhausted { budget },
                ),
                (None, RunStatus::Success, None) => (
                    surface::SurfaceTaskStatus::Completed,
                    surface::GenerationStopReason::Completed {
                        status: surface::GenerationCompletionStatus::Success,
                    },
                    surface::OperationTerminal::Succeeded {
                        usage: usage.clone(),
                    },
                ),
                (None, RunStatus::Cancelled, None) => (
                    surface::SurfaceTaskStatus::Cancelled,
                    surface::GenerationStopReason::Cancelled {
                        cause: surface::TerminalizationCause::UserCancel,
                    },
                    surface::OperationTerminal::Cancelled {
                        reason: surface::CancelReason::User,
                    },
                ),
                (None, RunStatus::ApprovalRequired, None) => (
                    surface::SurfaceTaskStatus::ApprovalRequired,
                    surface::GenerationStopReason::ProviderSuspended,
                    surface::OperationTerminal::Failed {
                        class: surface::FailureClass::LegacyApprovalRequired,
                        message: approval_message,
                    },
                ),
                (None, RunStatus::Failed | RunStatus::VerificationFailed, None) => (
                    surface::SurfaceTaskStatus::Failed,
                    surface::GenerationStopReason::ExecutionFailed {
                        class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                        message: failure_message.clone(),
                    },
                    surface::OperationTerminal::Failed {
                        class: surface::FailureClass::RuntimeInvariant,
                        message: failure_message,
                    },
                ),
            };
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let operation_id = typed.fence.operation_fence.operation_id.clone();
        let mut completion_events = outcome
            .response
            .as_ref()
            .map(|response| {
                self.generation_context_controller
                    .background_provider_response_events(
                        self.resident_surface.coordinator.state().snapshot(),
                        &typed.fence,
                        response,
                    )
            })
            .transpose()
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to prepare typed background provider response: {error}"),
            })?
            .unwrap_or_default();
        completion_events.extend([
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                    task_id: typed.task_id.clone(),
                    expected_revision: task.revision,
                    next_revision: next_task_revision,
                    status: task_status,
                    completed_at: (!approval_required)
                        .then_some(surface::UnixMillis::new(outcome.completed_at_ms)),
                    result: if committed_user_cancel {
                        Some(surface::DisplayText::new("Background turn cancelled"))
                    } else if outcome.status == RunStatus::Success {
                        Some(surface::DisplayText::new(outcome.status.as_str()))
                    } else {
                        None
                    },
                    error: if committed_user_cancel {
                        None
                    } else {
                        outcome.error.clone().map(surface::DisplayText::new)
                    },
                }),
            ),
            (
                surface::SurfaceScope::Background {
                    fence: typed.fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                    fence: typed.fence.operation_fence.clone(),
                    reason: stop_reason.clone(),
                    usage_delta: usage.clone(),
                }),
            ),
        ]);
        let background_approval = if approval_required {
            let tool = completion_events
                .iter()
                .find_map(|(_, event)| match event {
                    surface::SurfaceEvent::Tool(surface::ToolPatch::Requested { request }) => {
                        Some(request.clone())
                    }
                    _ => None,
                })
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "background approval lacks its committed tool request".to_string(),
                })?;
            let authority = Self::surface_authority_for_tool(
                self.resident_surface.coordinator.state().snapshot(),
                &typed.fence.operation_fence,
                &tool,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("background approval authority is invalid: {error}"),
            })?;
            let interaction_id =
                surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7");
            let revision = surface::InteractionRevision::try_new(1).expect("one is valid");
            let route_epoch = surface::ResponseRouteEpoch::try_new(1).expect("one is valid");
            let request = surface::SurfaceInteractionRequest::BackgroundApproval {
                task: surface::SurfaceTaskFence {
                    task_id: typed.task_id.clone(),
                    task_revision: next_task_revision,
                    background_owner: Some(typed.fence.clone()),
                },
                tool,
                authority,
            };
            let recovery_disposition =
                surface::InteractionUnavailableDisposition::AwaitCapableAttachment {
                    deadline: surface::InteractionExpiryDeadline {
                        issuing_host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        expires_at: surface::MonotonicInstant {
                            clock_id: surface::HostMonotonicClockId::try_from_bytes(
                                *uuid::Uuid::now_v7().as_bytes(),
                            )
                            .expect("generated UUID is v7"),
                            tick: surface::MonotonicTick::new(u64::MAX),
                        },
                        observed_expires_at: None,
                    },
                };
            let record = surface::BrokerInteractionRequestRecord {
                thread_id: typed.fence.operation_fence.thread_id.clone(),
                interaction_id: interaction_id.clone(),
                fence: typed.fence.operation_fence.clone(),
                kind: surface::SurfaceInteractionKind::BackgroundApproval,
                request: request.clone(),
                response_token: surface::SurfaceResponseToken::new(random_token_bytes()),
                answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
                recovery_disposition: recovery_disposition.clone(),
            };
            let route = surface::BrokerInteractionResponseRoute::Unassigned { epoch: route_epoch };
            let public_route = surface::SurfaceInteractionRoute::Unassigned { epoch: route_epoch };
            completion_events.extend([
                (
                    surface::SurfaceScope::Background {
                        fence: typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                        interaction: surface::SurfaceInteractionView {
                            interaction_id: interaction_id.clone(),
                            revision,
                            fence: typed.fence.operation_fence.clone(),
                            kind: surface::SurfaceInteractionKind::BackgroundApproval,
                            request,
                            route: public_route,
                            lifecycle: surface::SurfaceInteractionLifecycle::Requested,
                            recovery_disposition,
                        },
                    }),
                ),
                (
                    surface::SurfaceScope::Background {
                        fence: typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Suspended {
                        operation_id: operation_id.clone(),
                        cause: surface::SuspensionCause::ProviderSuspended {
                            generation_id: typed.fence.operation_fence.generation_id,
                        },
                    }),
                ),
            ]);
            Some(PreparedBackgroundApprovalInteraction {
                interaction_id,
                record,
                route,
                revision,
            })
        } else {
            completion_events.push((
                surface::SurfaceScope::Background {
                    fence: typed.fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal_commit_id: terminal_commit_id.clone(),
                    selected_cause: surface::OperationFinalizationCause::GenerationStop(
                        stop_reason,
                    ),
                    suspended_cause: None,
                    expected_settlements: Vec::new(),
                }),
            ));
            None
        };
        let completion_batch = self.surface_event_batch_with_commit_id(completion_events, None);
        Ok(PendingTypedProviderCompletion {
            typed,
            shutdown_reason,
            operation_id: operation_id.clone(),
            finalize_intent_id,
            terminal_commit_id,
            completion_batch,
            terminal,
            usage,
            terminal_batch: None,
            terminal_value: None,
            background_approval,
            stage: TypedProviderCompletionStage::Completion,
            rebuild_after_foreign_incomplete: false,
            retry_at: tokio::time::Instant::now(),
        })
    }

    pub(super) fn settle_typed_provider_completion(
        &mut self,
        pending: &mut PendingTypedProviderCompletion,
    ) -> Result<(), RuntimeHostError> {
        if pending.stage == TypedProviderCompletionStage::Completion {
            if self.resident_surface.coordinator.has_incomplete_batch()
                && !self
                    .resident_surface
                    .coordinator
                    .incomplete_batch_is(&pending.completion_batch)
            {
                pending.rebuild_after_foreign_incomplete = true;
                return Err(RuntimeHostError::ThreadStartFailed {
                    message:
                        "typed provider completion is waiting for another prepared surface batch"
                            .to_string(),
                });
            }
            let current_cursor = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .cursor
                .clone();
            if !self.resident_surface.coordinator.has_incomplete_batch()
                && (pending.rebuild_after_foreign_incomplete
                    || pending.completion_batch.cursor_before != current_cursor)
            {
                *pending = self.prepare_typed_provider_completion(
                    pending.typed.clone(),
                    pending.shutdown_reason.clone(),
                )?;
            }
            let completion = if pending.background_approval.is_some() {
                self.resident_surface
                    .coordinator
                    .commit_provider_background_suspend_batch(
                        pending.typed.fence.clone(),
                        &pending.completion_batch,
                    )
            } else {
                self.resident_surface
                    .coordinator
                    .commit_provider_background_stop_batch(
                        pending.typed.fence.clone(),
                        pending.operation_id.clone(),
                        pending.finalize_intent_id.clone(),
                        &pending.completion_batch,
                    )
            };
            completion.map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed provider completion: {error:?}"),
            })?;
            pending.stage = TypedProviderCompletionStage::Terminal;
        }
        if let Some(prepared) = pending.background_approval.take() {
            self.resident_surface.interactions.insert(
                prepared.interaction_id.clone(),
                ResidentSurfaceInteraction {
                    record: prepared.record,
                    route: prepared.route,
                    revision: prepared.revision,
                    waiter: None,
                    private_response: None,
                    pending_background_route: None,
                    winning_receipt: None,
                    resolution_ack: None,
                    projected_cursor: None,
                    cancelled: None,
                },
            );
            self.mirror_typed_provider_outcome_after_terminal(
                &pending.typed,
                pending.shutdown_reason.as_ref(),
            );
            return Ok(());
        }
        if self.resident_surface.coordinator.has_incomplete_batch()
            && !pending
                .terminal_batch
                .as_ref()
                .is_some_and(|batch| self.resident_surface.coordinator.incomplete_batch_is(batch))
        {
            return Err(RuntimeHostError::ThreadStartFailed {
                message: "typed provider terminal is waiting for another prepared surface batch"
                    .to_string(),
            });
        }
        let current_cursor = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .cursor
            .clone();
        let terminal_batch_is_stale = pending
            .terminal_batch
            .as_ref()
            .is_some_and(|batch| batch.cursor_before != current_cursor)
            && !self.resident_surface.coordinator.has_incomplete_batch();
        if pending.terminal_batch.is_none() || terminal_batch_is_stale {
            let terminal_batch = self.surface_event_batch_with_commit_id(
                vec![(
                    surface::SurfaceScope::Background {
                        fence: pending.typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal {
                        record: surface::OperationTerminalRecord {
                            operation_id: pending.operation_id.clone(),
                            finalize_intent_id: pending.finalize_intent_id.clone(),
                            terminal: pending.terminal.clone(),
                            usage: pending.usage.clone(),
                            source_diagnostic_digest: None,
                            settlement_receipts: Vec::new(),
                            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                                "background workflow terminal has no verifier proof",
                            ),
                            committed_at: surface::UnixMillis::new(
                                chrono::Utc::now().timestamp_millis(),
                            ),
                        },
                    }),
                )],
                Some(pending.terminal_commit_id.clone()),
            );
            pending.terminal_value = Some(surface::OperationTerminalAtCursor {
                operation_id: pending.operation_id.clone(),
                terminal: pending.terminal.clone(),
                completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                    "background workflow terminal has no verifier proof",
                ),
                cursor: terminal_batch.cursor_after.clone(),
                commit_class: terminal_batch.commit_class.clone(),
                batch_digest: terminal_batch.batch_digest.clone(),
            });
            pending.terminal_batch = Some(terminal_batch);
        }
        let terminal_batch = pending
            .terminal_batch
            .as_ref()
            .expect("typed provider terminal owns an exact batch");
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                pending.operation_id.clone(),
                pending.finalize_intent_id.clone(),
                terminal_batch,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed provider terminal: {error:?}"),
            })?;
        self.cache_surface_terminal(
            pending
                .terminal_value
                .clone()
                .expect("typed provider terminal owns its public value"),
        );
        self.mirror_typed_provider_outcome_after_terminal(
            &pending.typed,
            pending.shutdown_reason.as_ref(),
        );
        Ok(())
    }

    pub(super) fn mirror_typed_provider_outcome_after_terminal(
        &mut self,
        typed: &TypedProviderBackground,
        shutdown_reason: Option<&surface::SurfaceShutdownReason>,
    ) {
        let Some(outcome) = typed
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let already_terminal =
            typed
                .task_registry
                .get(typed.task_id.as_str())
                .is_some_and(|record| {
                    matches!(
                        record.status,
                        TaskStatus::Completed
                            | TaskStatus::Failed
                            | TaskStatus::Stopped
                            | TaskStatus::Cancelled
                            | TaskStatus::ApprovalRequired
                    )
                });
        if !already_terminal {
            if let Some(usage) = outcome.usage {
                self.usage_ledger.add(usage);
            }
            let budget_stopped = matches!(
                outcome.operation_terminal,
                Some(orca_core::budget::OperationTerminal::Stopped { .. })
            );
            let result = if budget_stopped {
                typed.task_registry.stop_with_usage(
                    typed.task_id.as_str(),
                    "budget_exhausted".to_string(),
                    outcome.usage,
                )
            } else {
                match (shutdown_reason, outcome.status) {
                    (Some(_), _) | (None, RunStatus::Cancelled) => {
                        typed.task_registry.stop_with_usage(
                            typed.task_id.as_str(),
                            outcome.status.as_str().to_string(),
                            outcome.usage,
                        )
                    }
                    (None, RunStatus::Success) => typed
                        .task_registry
                        .apply_main_session_terminal_update(
                            typed.task_id.as_str(),
                            MainSessionTerminalUpdate::Completed {
                                result: outcome.status.as_str().to_string(),
                            },
                            outcome.usage,
                        )
                        .map(|_| ()),
                    (None, RunStatus::ApprovalRequired) => match outcome.response.clone() {
                        Some(response) => typed
                            .task_registry
                            .approval_required_for_pending_provider_response_with_usage(
                                typed.task_id.as_str(),
                                outcome.status.as_str().to_string(),
                                response,
                                outcome.usage,
                            ),
                        None => {
                            Err("typed approval outcome lost its provider response".to_string())
                        }
                    },
                    (None, RunStatus::Failed | RunStatus::VerificationFailed) => typed
                        .task_registry
                        .apply_main_session_terminal_update(
                            typed.task_id.as_str(),
                            MainSessionTerminalUpdate::Failed {
                                error: outcome
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "background provider failed".to_string()),
                            },
                            outcome.usage,
                        )
                        .map(|_| ()),
                }
            };
            if let Err(error) = result {
                eprintln!(
                    "orca: typed provider terminal outpaced legacy task mirror for {}: {error}",
                    typed.task_id.as_str()
                );
            }
            if let Some(writer) = self
                .state
                .as_mut()
                .and_then(|state| state.thread.session_mut().writer_mut())
            {
                let _ = writer.append_background_task_provider_response(
                    typed.task_id.as_str(),
                    outcome.status.as_str(),
                    outcome.error.as_deref(),
                    outcome.usage,
                );
            }
        }
        if let Err(error) = typed
            .task_registry
            .clear_typed_provider_outcome(typed.task_id.as_str())
        {
            eprintln!(
                "orca: typed provider durable outcome cleanup failed for {}: {error}",
                typed.task_id.as_str()
            );
        }
    }

    pub(super) fn retry_typed_provider_completion(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = BackgroundRetryKey::ProviderCompletion(operation_id.clone());
        let Some(BackgroundRetryEffect::ProviderCompletion {
            operation_id,
            mut pending,
        }) = self.background_controller.begin_retry(&key)
        else {
            return;
        };
        let resolution = if self.settle_typed_provider_completion(&mut pending).is_err() {
            BackgroundRetryResolution::RetryAt(
                tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
            )
        } else {
            BackgroundRetryResolution::Settled
        };
        self.background_controller.resolve_retry(
            BackgroundRetryEffect::ProviderCompletion {
                operation_id,
                pending,
            },
            resolution,
        );
        if !self.background_controller.has_pending_completion() {
            self.operation_recovery.terminal_blocked = None;
        }
    }

    pub(super) fn retry_typed_provider_preparation(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = BackgroundRetryKey::ProviderPreparation(operation_id.clone());
        let Some(BackgroundRetryEffect::ProviderPreparation {
            operation_id,
            pending,
        }) = self.background_controller.begin_retry(&key)
        else {
            return;
        };
        match self.prepare_typed_provider_completion(
            pending.typed.clone(),
            pending.shutdown_reason.clone(),
        ) {
            Ok(mut completion) => {
                let settled = self
                    .settle_typed_provider_completion(&mut completion)
                    .is_ok();
                if !settled {
                    completion.retry_at =
                        tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                    self.background_controller
                        .retain_provider_completion(operation_id.clone(), completion);
                }
                self.background_controller.resolve_retry(
                    BackgroundRetryEffect::ProviderPreparation {
                        operation_id,
                        pending,
                    },
                    BackgroundRetryResolution::Settled,
                );
                if !self.background_controller.has_pending_completion() {
                    self.operation_recovery.terminal_blocked = None;
                }
            }
            Err(_) => self.background_controller.resolve_retry(
                BackgroundRetryEffect::ProviderPreparation {
                    operation_id,
                    pending,
                },
                BackgroundRetryResolution::RetryAt(
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                ),
            ),
        }
    }

    pub(super) fn commit_typed_workflow_completion(
        &mut self,
        typed: TypedWorkflowBackground,
        shutdown_reason: Option<surface::SurfaceShutdownReason>,
    ) -> Result<(), RuntimeHostError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == typed.task_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed workflow task disappeared before completion".to_string(),
            })?;
        let workflow = snapshot
            .workflows
            .iter()
            .find(|workflow| workflow.workflow_run_id == typed.workflow_run_id)
            .cloned()
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed workflow disappeared before completion".to_string(),
            })?;
        let record = self
            .state
            .as_ref()
            .and_then(|state| {
                state
                    .thread
                    .session()
                    .task_registry()
                    .get(typed.task_id.as_str())
            })
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "workflow task registry record disappeared before completion".to_string(),
            })?;
        let workflow_fence = surface::SurfaceWorkflowFence {
            workflow_run_id: workflow.workflow_run_id.clone(),
            workflow_revision: workflow.revision,
            parent: workflow.parent.clone(),
        };
        let next_task_revision =
            surface::TaskRevision::try_new(task.revision.get().checked_add(1).ok_or_else(
                || RuntimeHostError::ThreadStartFailed {
                    message: "workflow task revision exhausted".to_string(),
                },
            )?)
            .map_err(|_| RuntimeHostError::ThreadStartFailed {
                message: "workflow task revision is invalid".to_string(),
            })?;
        let next_workflow_revision =
            surface::WorkflowRevision::try_new(workflow.revision.get().checked_add(1).ok_or_else(
                || RuntimeHostError::ThreadStartFailed {
                    message: "workflow revision exhausted".to_string(),
                },
            )?)
            .map_err(|_| RuntimeHostError::ThreadStartFailed {
                message: "workflow revision is invalid".to_string(),
            })?;
        let second_workflow_revision = surface::WorkflowRevision::try_new(
            next_workflow_revision.get().checked_add(1).ok_or_else(|| {
                RuntimeHostError::ThreadStartFailed {
                    message: "workflow result revision exhausted".to_string(),
                }
            })?,
        )
        .map_err(|_| RuntimeHostError::ThreadStartFailed {
            message: "workflow second revision is invalid".to_string(),
        })?;
        let usage = surface_usage_totals(record.usage.unwrap_or_default());
        let diagnostic = surface::SafeDiagnosticText::try_new("background workflow failed")
            .expect("fixed diagnostic is bounded");
        let display_reason = surface::DisplayText::new(
            record
                .error
                .clone()
                .or_else(|| record.result.clone())
                .unwrap_or_else(|| "Workflow stopped".to_string()),
        );
        let projected_status = if workflow.status == surface::SurfaceWorkflowStatus::Stopping {
            TaskStatus::Stopped
        } else {
            record.status
        };
        let (
            task_status,
            workflow_patches,
            terminal_workflow_revision,
            result_status,
            result_content,
            stop_reason,
            terminal,
        ) = match projected_status {
            TaskStatus::Completed => (
                surface::SurfaceTaskStatus::Completed,
                vec![surface::WorkflowPatch::Completed {
                    fence: workflow_fence.clone(),
                    next_revision: next_workflow_revision,
                }],
                next_workflow_revision,
                surface::SurfaceWorkflowResultStatus::Success,
                surface::DisplayText::new(
                    record
                        .result
                        .clone()
                        .unwrap_or_else(|| "Workflow completed".to_string()),
                ),
                surface::GenerationStopReason::Completed {
                    status: surface::GenerationCompletionStatus::Success,
                },
                surface::OperationTerminal::Succeeded {
                    usage: usage.clone(),
                },
            ),
            TaskStatus::Stopped => {
                let (patches, terminal_revision) =
                    if workflow.status == surface::SurfaceWorkflowStatus::Stopping {
                        (
                            vec![surface::WorkflowPatch::Stopped {
                                fence: workflow_fence.clone(),
                                next_revision: next_workflow_revision,
                                reason: display_reason.clone(),
                            }],
                            next_workflow_revision,
                        )
                    } else {
                        (
                            vec![
                                surface::WorkflowPatch::Stopping {
                                    fence: workflow_fence.clone(),
                                    next_revision: next_workflow_revision,
                                    reason: display_reason.clone(),
                                },
                                surface::WorkflowPatch::Stopped {
                                    fence: surface::SurfaceWorkflowFence {
                                        workflow_run_id: workflow.workflow_run_id.clone(),
                                        workflow_revision: next_workflow_revision,
                                        parent: workflow.parent.clone(),
                                    },
                                    next_revision: second_workflow_revision,
                                    reason: display_reason.clone(),
                                },
                            ],
                            second_workflow_revision,
                        )
                    };
                (
                    surface::SurfaceTaskStatus::Stopped,
                    patches,
                    terminal_revision,
                    surface::SurfaceWorkflowResultStatus::Failed,
                    display_reason.clone(),
                    shutdown_reason.map_or(
                        surface::GenerationStopReason::Cancelled {
                            cause: surface::TerminalizationCause::UserCancel,
                        },
                        |reason| surface::GenerationStopReason::Cancelled {
                            cause: match reason {
                                surface::SurfaceShutdownReason::HostShutdown => {
                                    surface::TerminalizationCause::HostShutdown
                                }
                                surface::SurfaceShutdownReason::ThreadClose => {
                                    surface::TerminalizationCause::ThreadClose
                                }
                            },
                        },
                    ),
                    shutdown_reason.map_or(
                        surface::OperationTerminal::Cancelled {
                            reason: surface::CancelReason::User,
                        },
                        |reason| surface::OperationTerminal::Shutdown { reason },
                    ),
                )
            }
            TaskStatus::Cancelled => (
                surface::SurfaceTaskStatus::Cancelled,
                vec![surface::WorkflowPatch::Cancelled {
                    fence: workflow_fence.clone(),
                    next_revision: next_workflow_revision,
                    reason: display_reason.clone(),
                }],
                next_workflow_revision,
                surface::SurfaceWorkflowResultStatus::Failed,
                display_reason.clone(),
                surface::GenerationStopReason::Cancelled {
                    cause: surface::TerminalizationCause::UserCancel,
                },
                surface::OperationTerminal::Cancelled {
                    reason: surface::CancelReason::User,
                },
            ),
            _ => (
                surface::SurfaceTaskStatus::Failed,
                vec![surface::WorkflowPatch::Failed {
                    fence: workflow_fence.clone(),
                    next_revision: next_workflow_revision,
                    error: display_reason.clone(),
                }],
                next_workflow_revision,
                surface::SurfaceWorkflowResultStatus::Failed,
                display_reason,
                surface::GenerationStopReason::ExecutionFailed {
                    class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                    message: diagnostic.clone(),
                },
                surface::OperationTerminal::Failed {
                    class: surface::FailureClass::RuntimeInvariant,
                    message: diagnostic,
                },
            ),
        };
        let result_workflow_revision = surface::WorkflowRevision::try_new(
            terminal_workflow_revision
                .get()
                .checked_add(1)
                .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                    message: "workflow result revision exhausted".to_string(),
                })?,
        )
        .map_err(|_| RuntimeHostError::ThreadStartFailed {
            message: "workflow result revision is invalid".to_string(),
        })?;
        let result = surface::SurfaceWorkflowResult {
            result_id: surface::SurfaceWorkflowResultId::try_new(format!(
                "workflow-result-{}",
                typed.workflow_run_id.as_str()
            ))
            .map_err(|_| RuntimeHostError::ThreadStartFailed {
                message: "workflow result identity is invalid".to_string(),
            })?,
            tool_use_id: Some(typed.tool_use_id.clone()),
            status: result_status,
            content: result_content,
            acknowledged_by_operation: None,
        };
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let operation_id = typed.fence.operation_fence.operation_id.clone();
        let mut completion_events = vec![(
            surface::SurfaceScope::Thread,
            surface::SurfaceEvent::Task(surface::TaskPatch::StatusChanged {
                task_id: typed.task_id.clone(),
                expected_revision: task.revision,
                next_revision: next_task_revision,
                status: task_status,
                completed_at: record.completed_at_ms.map(surface::UnixMillis::new),
                result: record.result.map(surface::DisplayText::new),
                error: record.error.map(surface::DisplayText::new),
            }),
        )];
        completion_events.extend(workflow_patches.into_iter().map(|patch| {
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Workflow(patch),
            )
        }));
        completion_events.extend([
            (
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Workflow(surface::WorkflowPatch::ResultReady {
                    fence: surface::SurfaceWorkflowFence {
                        workflow_run_id: workflow.workflow_run_id,
                        workflow_revision: terminal_workflow_revision,
                        parent: workflow.parent,
                    },
                    next_revision: result_workflow_revision,
                    result,
                }),
            ),
            (
                surface::SurfaceScope::Background {
                    fence: typed.fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                    fence: typed.fence.operation_fence.clone(),
                    reason: stop_reason.clone(),
                    usage_delta: usage.clone(),
                }),
            ),
            (
                surface::SurfaceScope::Background {
                    fence: typed.fence.clone(),
                },
                surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal_commit_id: terminal_commit_id.clone(),
                    selected_cause: surface::OperationFinalizationCause::GenerationStop(
                        stop_reason,
                    ),
                    suspended_cause: None,
                    expected_settlements: Vec::new(),
                }),
            ),
        ]);
        let completion_batch = self.surface_event_batch_with_commit_id(completion_events, None);
        let mut pending = PendingTypedWorkflowCompletion {
            typed,
            operation_id: operation_id.clone(),
            finalize_intent_id,
            terminal_commit_id,
            completion_batch,
            terminal,
            usage,
            terminal_batch: None,
            terminal_value: None,
            stage: TypedWorkflowCompletionStage::Completion,
            retry_at: tokio::time::Instant::now(),
        };
        match self.settle_typed_workflow_completion(&mut pending) {
            Ok(()) => Ok(()),
            Err(error) => {
                pending.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                self.background_controller
                    .retain_workflow_completion(operation_id, pending);
                Err(error)
            }
        }
    }

    pub(super) fn settle_typed_workflow_completion(
        &mut self,
        pending: &mut PendingTypedWorkflowCompletion,
    ) -> Result<(), RuntimeHostError> {
        if pending.stage == TypedWorkflowCompletionStage::Completion {
            self.resident_surface
                .coordinator
                .commit_workflow_background_stop_batch(
                    pending.typed.fence.clone(),
                    pending.operation_id.clone(),
                    pending.finalize_intent_id.clone(),
                    &pending.completion_batch,
                )
                .map_err(|error| RuntimeHostError::ThreadStartFailed {
                    message: format!("failed to commit typed workflow completion: {error:?}"),
                })?;
            pending.stage = TypedWorkflowCompletionStage::Terminal;
        }
        if pending.terminal_batch.is_none() {
            let terminal_batch = self.surface_event_batch_with_commit_id(
                vec![(
                    surface::SurfaceScope::Background {
                        fence: pending.typed.fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal {
                        record: surface::OperationTerminalRecord {
                            operation_id: pending.operation_id.clone(),
                            finalize_intent_id: pending.finalize_intent_id.clone(),
                            terminal: pending.terminal.clone(),
                            usage: pending.usage.clone(),
                            source_diagnostic_digest: None,
                            settlement_receipts: Vec::new(),
                            completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                                "background workflow terminal has no verifier proof",
                            ),
                            committed_at: surface::UnixMillis::new(
                                chrono::Utc::now().timestamp_millis(),
                            ),
                        },
                    }),
                )],
                Some(pending.terminal_commit_id.clone()),
            );
            pending.terminal_value = Some(surface::OperationTerminalAtCursor {
                operation_id: pending.operation_id.clone(),
                terminal: pending.terminal.clone(),
                completion_proof: surface::SurfaceOperationCompletionProof::unverified(
                    "background workflow terminal has no verifier proof",
                ),
                cursor: terminal_batch.cursor_after.clone(),
                commit_class: terminal_batch.commit_class.clone(),
                batch_digest: terminal_batch.batch_digest.clone(),
            });
            pending.terminal_batch = Some(terminal_batch);
        }
        let terminal_batch = pending
            .terminal_batch
            .as_ref()
            .expect("terminal stage owns an exact terminal batch");
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                pending.operation_id.clone(),
                pending.finalize_intent_id.clone(),
                terminal_batch,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed workflow terminal: {error:?}"),
            })?;
        self.cache_surface_terminal(
            pending
                .terminal_value
                .clone()
                .expect("terminal batch owns its public terminal value"),
        );
        Ok(())
    }

    pub(super) fn retry_typed_workflow_completion(
        &mut self,
        operation_id: &surface::SurfaceOperationId,
    ) {
        let key = BackgroundRetryKey::WorkflowCompletion(operation_id.clone());
        let Some(BackgroundRetryEffect::WorkflowCompletion {
            operation_id,
            mut pending,
        }) = self.background_controller.begin_retry(&key)
        else {
            return;
        };
        let resolution = if self.settle_typed_workflow_completion(&mut pending).is_err() {
            BackgroundRetryResolution::RetryAt(
                tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
            )
        } else {
            BackgroundRetryResolution::Settled
        };
        self.background_controller.resolve_retry(
            BackgroundRetryEffect::WorkflowCompletion {
                operation_id,
                pending,
            },
            resolution,
        );
        if !self.background_controller.has_pending_completion() {
            self.operation_recovery.terminal_blocked = None;
        }
    }

    pub(super) async fn shutdown_background_tasks(
        &mut self,
        reason: surface::SurfaceShutdownReason,
    ) -> Result<(), RuntimeHostError> {
        let tasks = self.background_controller.begin_shutdown();
        let mut first_error = None;
        for task in tasks {
            let HostBackgroundTask {
                join,
                typed_workflow,
                typed_provider,
                ..
            } = task;
            let _ = join.await;
            if let Some(typed_workflow) = typed_workflow {
                if let Err(error) =
                    self.commit_typed_workflow_completion(typed_workflow, Some(reason))
                {
                    self.operation_recovery.terminal_blocked = Some(error.to_string());
                    first_error.get_or_insert(error);
                }
            }
            if let Some(typed_provider) = typed_provider
                && let Err(error) =
                    self.commit_typed_provider_completion(typed_provider, Some(reason))
            {
                self.operation_recovery.terminal_blocked = Some(error.to_string());
                first_error.get_or_insert(error);
            }
        }
        for _ in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            if !self.background_controller.has_pending_completion() {
                return Ok(());
            }
            let operation_ids = self
                .background_controller
                .pending_completion_operation_ids();
            for operation_id in operation_ids {
                self.retry_typed_workflow_completion(&operation_id);
                self.retry_typed_provider_preparation(&operation_id);
                self.retry_typed_provider_completion(&operation_id);
            }
            if self.background_controller.has_pending_completion() {
                tokio::time::sleep(SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL).await;
            }
        }
        first_error.map_or_else(
            || {
                Err(RuntimeHostError::ThreadStartFailed {
                    message: "typed background completion retry did not converge".to_string(),
                })
            },
            Err,
        )
    }
}
