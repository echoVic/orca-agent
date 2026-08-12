use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::RunStatus;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::tool_types::ToolRequest;

use crate::child_agent_entrypoints::run_child_agent_with_executor;
use crate::child_agent_loop_setup::{
    ChildAgentLoopSetup, ChildAgentTurnBudget, advance_child_agent_turn, prepare_child_agent_loop,
};
use crate::child_agent_provider_turn::{
    ChildAgentProviderErrorDecision, ChildAgentProviderTurn,
    compact_child_agent_conversation_if_needed, handle_child_agent_provider_error,
    route_child_agent_model, run_child_agent_provider_turn, run_child_agent_provider_turn_observed,
};
use crate::child_agent_response_folding::{
    ChildAgentProviderResponseFold, ChildAgentToolContext, ChildAgentToolExecution,
    ChildAgentToolResultFold, child_agent_tool_requests, fold_child_agent_provider_response,
    fold_child_agent_tool_result_and_close_siblings,
};
use crate::child_agent_types::{
    ChildAgentActivity, ChildAgentActivityObserver, ChildAgentRequest, ChildAgentResult,
};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::run_status_from_tool_status;
use crate::memory::MemoryBlock;

pub struct ChildAgentLoopContext<'a> {
    pub request: &'a ChildAgentRequest,
    pub cwd: &'a Path,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub hooks: &'a HookRunner,
    pub child_cost_tracker: &'a mut CostTracker,
    /// Parent budget lease bounding this child's admission; `None` (tests and
    /// legacy callers) falls back to an unlimited lease derived from config.
    pub lease: Option<&'a mut crate::budget_controller::BudgetLease>,
}

pub fn run_child_agent_loop_with_tool_executor<F>(
    config: &RunConfig,
    context: ChildAgentLoopContext<'_>,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(
        config,
        context.request,
        context.cwd,
        context.instructions,
        context.memory,
    );
    let mut fallback_lease =
        crate::budget_controller::BudgetController::new(config.budget.to_spec())
            .child_lease(config.budget.to_spec())
            .expect("child lease derives from the child config budget");
    let mut lease = match context.lease {
        Some(lease) => lease,
        None => &mut fallback_lease,
    };
    loop {
        match advance_child_agent_turn(&mut setup, &mut lease) {
            ChildAgentTurnBudget::Continue => {}
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, context.cwd, context.hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, context.request, &setup, context.child_cost_tracker);

        let response = match run_child_agent_provider_turn(
            config,
            &setup,
            context.cwd,
            context.hooks,
            &turn_provider_config,
            &child_cancel,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail { result, usage } => {
                record_child_provider_usage(usage, context.child_cost_tracker, None);
                if let Some(result) =
                    child_agent_budget_exhausted_result(config, context.child_cost_tracker)
                {
                    return Ok(result);
                }
                return Ok(result);
            }
        };

        let provider_error_decision = handle_child_agent_provider_error_with_usage(
            config,
            &mut setup,
            context.cwd,
            context.hooks,
            &response,
            context.child_cost_tracker,
            None,
        )?;
        if let Some(result) =
            child_agent_budget_exhausted_result(config, context.child_cost_tracker)
        {
            return Ok(result);
        }
        match provider_error_decision {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        let provider_fold =
            fold_child_agent_provider_response(&mut setup, &response, context.child_cost_tracker);
        if let Some(result) =
            child_agent_budget_exhausted_result(config, context.child_cost_tracker)
        {
            return Ok(result);
        }
        match provider_fold {
            ChildAgentProviderResponseFold::Complete(result) => return Ok(result),
            ChildAgentProviderResponseFold::ContinueToTools => {}
        }

        let tool_requests = child_agent_tool_requests(&response);
        for (index, tool_request) in tool_requests.iter().enumerate() {
            let tool_context = ChildAgentToolContext {
                policy: &setup.policy,
                mcp_registry: &setup.mcp_registry,
            };
            let tool_execution = execute_tool(&tool_context, &child_cancel, tool_request);
            let tool_fold = fold_child_agent_tool_result_and_close_siblings(
                &mut setup,
                tool_request,
                &tool_requests[index + 1..],
                tool_execution.should_stop,
                tool_execution.result,
                tool_execution.child_cost,
                context.child_cost_tracker,
            );
            if let Some(result) =
                child_agent_budget_exhausted_result(config, context.child_cost_tracker)
            {
                return Ok(result);
            }
            match tool_fold {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
        }
    }
}

pub fn run_child_agent_loop_with_tool_executor_observed<F>(
    config: &RunConfig,
    context: ChildAgentLoopContext<'_>,
    observer: Option<&ChildAgentActivityObserver<'_>>,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(
        config,
        context.request,
        context.cwd,
        context.instructions,
        context.memory,
    );
    let mut fallback_lease =
        crate::budget_controller::BudgetController::new(config.budget.to_spec())
            .child_lease(config.budget.to_spec())
            .expect("child lease derives from the child config budget");
    let mut lease = match context.lease {
        Some(lease) => lease,
        None => &mut fallback_lease,
    };
    loop {
        match advance_child_agent_turn(&mut setup, &mut lease) {
            ChildAgentTurnBudget::Continue => {
                if let Some(observer) = observer {
                    observer.emit(ChildAgentActivity::TurnStarted { turn: setup.turn });
                }
            }
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, context.cwd, context.hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, context.request, &setup, context.child_cost_tracker);

        let response = match run_child_agent_provider_turn_observed(
            config,
            &setup,
            context.cwd,
            context.hooks,
            &turn_provider_config,
            &child_cancel,
            observer,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail { result, usage } => {
                record_child_provider_usage(usage, context.child_cost_tracker, observer);
                if let Some(result) =
                    child_agent_budget_exhausted_result(config, context.child_cost_tracker)
                {
                    return Ok(result);
                }
                return Ok(result);
            }
        };

        let provider_error_decision = handle_child_agent_provider_error_with_usage(
            config,
            &mut setup,
            context.cwd,
            context.hooks,
            &response,
            context.child_cost_tracker,
            observer,
        )?;
        if let Some(result) =
            child_agent_budget_exhausted_result(config, context.child_cost_tracker)
        {
            return Ok(result);
        }
        match provider_error_decision {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        let had_usage = response
            .usage
            .as_ref()
            .is_some_and(|usage| !usage.is_empty());
        let provider_fold =
            fold_child_agent_provider_response(&mut setup, &response, context.child_cost_tracker);
        if had_usage {
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::Usage(
                    context.child_cost_tracker.totals(),
                ));
            }
        }
        if let Some(result) =
            child_agent_budget_exhausted_result(config, context.child_cost_tracker)
        {
            return Ok(result);
        }
        match provider_fold {
            ChildAgentProviderResponseFold::Complete(result) => return Ok(result),
            ChildAgentProviderResponseFold::ContinueToTools => {}
        }

        let tool_requests = child_agent_tool_requests(&response);
        for (index, tool_request) in tool_requests.iter().enumerate() {
            let tool_context = ChildAgentToolContext {
                policy: &setup.policy,
                mcp_registry: &setup.mcp_registry,
            };
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::ToolStarted {
                    name: tool_request.name.as_str().to_string(),
                    target: tool_request.target.clone(),
                });
            }
            let tool_execution = execute_tool(&tool_context, &child_cancel, tool_request);
            let had_child_cost = tool_execution.child_cost.is_some();
            if let Some(observer) = observer {
                observer.emit(ChildAgentActivity::ToolCompleted {
                    name: tool_request.name.as_str().to_string(),
                    status: run_status_from_tool_status(tool_execution.result.status),
                });
            }
            let tool_fold = fold_child_agent_tool_result_and_close_siblings(
                &mut setup,
                tool_request,
                &tool_requests[index + 1..],
                tool_execution.should_stop,
                tool_execution.result,
                tool_execution.child_cost,
                context.child_cost_tracker,
            );
            if had_child_cost {
                if let Some(observer) = observer {
                    observer.emit(ChildAgentActivity::Usage(
                        context.child_cost_tracker.totals(),
                    ));
                }
            }
            if let Some(result) =
                child_agent_budget_exhausted_result(config, context.child_cost_tracker)
            {
                return Ok(result);
            }
            match tool_fold {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
        }
    }
}

pub(crate) fn handle_child_agent_provider_error_with_usage(
    config: &RunConfig,
    setup: &mut ChildAgentLoopSetup,
    cwd: &Path,
    hooks: &HookRunner,
    response: &ProviderResponse,
    child_cost_tracker: &mut CostTracker,
    observer: Option<&ChildAgentActivityObserver<'_>>,
) -> io::Result<Option<ChildAgentProviderErrorDecision>> {
    let has_provider_error = response
        .steps
        .iter()
        .any(|step| matches!(step, ProviderStep::Error(_)));
    if has_provider_error {
        record_child_provider_usage(response.usage, child_cost_tracker, observer);
    }

    handle_child_agent_provider_error(config, setup, cwd, hooks, response)
}

fn record_child_provider_usage(
    usage: Option<orca_core::provider_types::Usage>,
    child_cost_tracker: &mut CostTracker,
    observer: Option<&ChildAgentActivityObserver<'_>>,
) {
    let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
        return;
    };
    child_cost_tracker.add_usage(usage);
    if let Some(observer) = observer {
        observer.emit(ChildAgentActivity::Usage(child_cost_tracker.totals()));
    }
}

pub(crate) fn child_agent_budget_exhausted_result(
    config: &RunConfig,
    child_cost_tracker: &CostTracker,
) -> Option<ChildAgentResult> {
    let max_cost_usd_micros = config.budget.max_cost_usd_micros?;
    let spent_usd_micros =
        crate::cost::usd_to_micros(child_cost_tracker.totals().estimated_cost_usd);
    (spent_usd_micros > max_cost_usd_micros).then(|| ChildAgentResult {
        status: RunStatus::Failed,
        final_message: None,
        error: Some(format!(
            "budget stopped: estimated cost ${:.6} exceeded limit ${:.6}",
            spent_usd_micros as f64 / 1_000_000.0,
            max_cost_usd_micros as f64 / 1_000_000.0
        )),
    })
}

#[cfg(test)]
mod tests {
    use crate::lifecycle::run_status_from_tool_status;
    use orca_core::event_schema::RunStatus;
    use orca_core::tool_types::ToolStatus;

    #[test]
    fn child_agent_tool_terminal_status_preserves_cancelled_and_unknown_outcomes() {
        assert_eq!(
            run_status_from_tool_status(ToolStatus::Cancelled),
            RunStatus::Cancelled
        );
        assert_eq!(
            run_status_from_tool_status(ToolStatus::Indeterminate),
            RunStatus::Failed
        );
    }
}

pub fn run_child_agent_with_tool_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    mut execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    run_child_agent_with_executor(config, request, |config, request, child_cost_tracker| {
        run_child_agent_loop_with_tool_executor(
            config,
            ChildAgentLoopContext {
                request,
                cwd,
                instructions,
                memory,
                hooks,
                child_cost_tracker,
                lease: None,
            },
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}

pub fn run_child_agent_with_tool_executor_observed<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    observer: Option<&ChildAgentActivityObserver<'_>>,
    mut execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    run_child_agent_with_executor(config, request, |config, request, child_cost_tracker| {
        run_child_agent_loop_with_tool_executor_observed(
            config,
            ChildAgentLoopContext {
                request,
                cwd,
                instructions,
                memory,
                hooks,
                child_cost_tracker,
                lease: None,
            },
            observer,
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}
