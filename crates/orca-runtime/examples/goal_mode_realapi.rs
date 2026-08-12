//! Real DeepSeek regressions for the runtime-owned Goal control plane.
//!
//! Makes billed API calls. The caller should provide an isolated ORCA_HOME
//! containing auth.json, or set DEEPSEEK_API_KEY directly.

use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
    ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::conversation::Message;
use orca_core::event_schema::{EventEnvelope, EventType, RunStatus};
use orca_core::event_sink::EventObserver;
use orca_core::goal_runtime::{GoalId, GoalPauseReason, GoalState, GoalTurnOrigin};
use orca_core::model::{FLASH_MODEL, ModelSelection};
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::goal_store::{CreateGoalInput, GoalStore};
use orca_runtime::runtime_host::{
    HostedOperationKind, HostedTurnRequest, OperationOutcome, PauseGoalRunResult, RuntimeHost,
    RuntimeThreadHandle,
};

const COMPLETION_OBJECTIVE: &str =
    "Inspect the current runtime task list once, then mark this goal complete.";
const COMPLETION_PROMPT: &str = "Complete the active goal now. Make exactly two sequential tool calls and no others: first call task_list with {}, wait for its result, then call update_goal with {\"status\":\"complete\",\"reason\":\"task list was inspected\",\"evidence\":[{\"kind\":\"observation\",\"summary\":\"task_list completed successfully\"}],\"blocker\":null}. Do not answer in prose before the tool calls.";
const REJECTED_OBJECTIVE: &str =
    "Demonstrate that a completion intent without evidence is rejected by the Goal runtime.";
const REJECTED_PROMPT: &str = "This is a control-plane rejection test. Call update_goal exactly once with {\"status\":\"complete\",\"reason\":\"intentionally missing evidence\",\"evidence\":[],\"blocker\":null}. After the tool reports rejection, do not retry and do not call another tool; answer with exactly REJECTED_COMPLETION_OBSERVED.";
const BLOCKED_OBJECTIVE: &str = "Read the non-empty value of environment variable ORCA_GOAL_REALAPI_REQUIRED_EXTERNAL_STATE_DOES_NOT_EXIST. The harness intentionally does not provide it and no alternative source is authorized.";
const BLOCKED_PROMPT: &str = "Establish the external blocker, then report it. First call bash with {\"command\":\"if [ -n \\\"$ORCA_GOAL_REALAPI_REQUIRED_EXTERNAL_STATE_DOES_NOT_EXIST\\\" ]; then printf PRESENT; else printf MISSING; fi\"}. After it prints MISSING, call update_goal with {\"status\":\"blocked\",\"reason\":\"required external state is absent\",\"evidence\":[{\"kind\":\"observation\",\"summary\":\"bash printed MISSING for the required environment variable\",\"target\":\"ORCA_GOAL_REALAPI_REQUIRED_EXTERNAL_STATE_DOES_NOT_EXIST\"}],\"blocker\":{\"kind\":\"external_state\",\"summary\":\"required environment variable is absent\"}}. Do not invent an alternative.";
const CANCELLATION_OBJECTIVE: &str =
    "Start the requested long-running shell check and wait for it to finish.";
const CANCELLATION_PROMPT: &str = "Call bash exactly once with {\"command\":\"sleep 30\"}. Do not call update_goal and do not answer before the command finishes.";

#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<EventEnvelope>>,
    changed: Condvar,
}

impl RecordingObserver {
    fn events(&self) -> Vec<EventEnvelope> {
        self.events.lock().expect("goal event observer").clone()
    }

    fn wait_for_tool(&self, name: &str, timeout: Duration) -> bool {
        let events = self.events.lock().expect("goal event observer");
        let (events, _) = self
            .changed
            .wait_timeout_while(events, timeout, |events| {
                !events.iter().any(|event| {
                    event.event_type == EventType::ToolCallRequested
                        && event.payload["name"].as_str() == Some(name)
                })
            })
            .expect("wait for Goal real API tool event");
        events.iter().any(|event| {
            event.event_type == EventType::ToolCallRequested
                && event.payload["name"].as_str() == Some(name)
        })
    }
}

impl EventObserver for RecordingObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        self.events
            .lock()
            .expect("goal event observer")
            .push(event.clone());
        self.changed.notify_all();
        Ok(())
    }
}

#[derive(Debug)]
struct ScenarioAudit {
    state: &'static str,
    reason: String,
    pause_reason: Option<&'static str>,
    rejection_code: Option<String>,
    outer_turns: i64,
    update_goal_requests: usize,
    update_goal_acks: usize,
    accepted_acks: usize,
    rejected_acks: usize,
    persisted_intents: i64,
    verifier_outcomes: usize,
    verifier_tokens: i64,
    usage_events: i64,
    charged_tokens: i64,
    cost_micros: i64,
    journal_goal_events: usize,
    continuations: usize,
    stale_continuations: usize,
    in_flight_runs: i64,
    resume_turns: usize,
}

impl ScenarioAudit {
    fn common_metrics(&self) -> String {
        format!(
            "outer_turns={} update_goal_requests={} update_goal_acks={} accepted_acks={} rejected_acks={} persisted_intents={} verifier_outcomes={} verifier_tokens={} usage_events={} charged_tokens={} cost_micros={} journal_goal_events={} continuations={} stale_continuations={} in_flight_runs={}",
            self.outer_turns,
            self.update_goal_requests,
            self.update_goal_acks,
            self.accepted_acks,
            self.rejected_acks,
            self.persisted_intents,
            self.verifier_outcomes,
            self.verifier_tokens,
            self.usage_events,
            self.charged_tokens,
            self.cost_micros,
            self.journal_goal_events,
            self.continuations,
            self.stale_continuations,
            self.in_flight_runs,
        )
    }
}

fn is_goal_event(event: &EventEnvelope) -> bool {
    matches!(
        event.event_type,
        EventType::GoalCreated
            | EventType::GoalRunStarted
            | EventType::GoalTurnStarted
            | EventType::GoalIntentRequested
            | EventType::GoalIntentAcknowledged
            | EventType::GoalTurnFinished
            | EventType::GoalVerificationCompleted
            | EventType::GoalTransitioned
            | EventType::GoalContinuationAdmitted
            | EventType::GoalContinuationRejected
            | EventType::GoalPaused
            | EventType::GoalRecovered
            | EventType::GoalCompleted
    )
}

fn state_name(state: &GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Paused { .. } => "paused",
        GoalState::Blocked { .. } => "blocked",
        GoalState::BudgetLimited => "budget_limited",
        GoalState::Complete { .. } => "complete",
    }
}

fn pause_reason_name(reason: GoalPauseReason) -> &'static str {
    match reason {
        GoalPauseReason::User => "user",
        GoalPauseReason::NoProgress => "no_progress",
        GoalPauseReason::Backoff => "backoff",
        GoalPauseReason::Infrastructure => "infrastructure",
        GoalPauseReason::WaitingForWorkflow => "waiting_for_workflow",
        GoalPauseReason::Recovery => "recovery",
        GoalPauseReason::UsageLimit => "usage_limit",
    }
}

fn parse_max_budget() -> Result<u64, String> {
    let mut args = std::env::args().skip(1);
    let mut max_budget = 0.02;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max-budget" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--max-budget requires a value".to_string())?;
                max_budget = value
                    .parse::<f64>()
                    .map_err(|error| format!("invalid --max-budget value '{value}': {error}"))?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if !max_budget.is_finite() || max_budget <= 0.0 {
        return Err("--max-budget must be a positive finite number".to_string());
    }
    Ok((max_budget * 1_000_000.0).round() as u64)
}

fn load_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let home = std::env::var_os("ORCA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))?;
    let content = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let auth: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    auth.get("DEEPSEEK_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
}

fn real_api_config(api_key: String, max_cost_usd_micros: u64) -> Result<RunConfig, String> {
    Ok(RunConfig {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(std::env::current_dir().map_err(|error| error.to_string())?),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::DeepSeek,
        verifier: None,
        model: ModelSelection::parse(Some(FLASH_MODEL.to_string()))?,
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Max,
        api_key: Some(api_key),
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: orca_core::config::BudgetConfig {
            max_cost_usd_micros: Some(max_cost_usd_micros),
            ..orca_core::config::BudgetConfig::default()
        },
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::Dark,
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    })
}

fn fail(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn create_goal(
    thread: &RuntimeThreadHandle,
    objective: &str,
) -> Result<(String, GoalId), Box<dyn Error>> {
    let session_id = thread
        .session_id()
        .ok_or_else(|| fail("recorded RuntimeHost thread did not expose a session id"))?
        .to_string();
    let runtime = thread
        .goal_runtime()
        .map_err(|error| fail(error.to_string()))?;
    let goal = runtime
        .create(CreateGoalInput {
            session_id: session_id.clone(),
            objective: objective.to_string(),
            token_budget: None,
            now: chrono::Utc::now().timestamp(),
        })
        .map_err(|error| fail(error.to_string()))?;
    Ok((session_id, goal.goal_id))
}

fn start_goal_run(
    thread: &RuntimeThreadHandle,
    prompt: &str,
    observer: Arc<RecordingObserver>,
    origin: GoalTurnOrigin,
) -> Result<orca_runtime::runtime_host::OperationHandle, Box<dyn Error>> {
    thread
        .start_turn(
            HostedTurnRequest::new(prompt)
                .with_operation_kind(HostedOperationKind::GoalRun)
                .with_goal_tools(true)
                .with_goal_usage_tracking(true)
                .with_goal_turn_origin(origin)
                .with_event_observer(observer),
            io::sink(),
        )
        .map_err(|error| fail(error.to_string()))
}

fn require_outcome(
    outcome: &OperationOutcome,
    expected: RunStatus,
    scenario: &str,
) -> Result<(), Box<dyn Error>> {
    if outcome != &OperationOutcome::Completed(expected) {
        return Err(fail(format!(
            "Goal Mode {scenario} operation had unexpected outcome: {outcome:?}"
        )));
    }
    Ok(())
}

fn tool_names(thread: &RuntimeThreadHandle) -> Result<Vec<String>, Box<dyn Error>> {
    let snapshot = thread.snapshot().map_err(|error| fail(error.to_string()))?;
    Ok(snapshot
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|call| call.function_name.clone())
        .collect())
}

fn require_tools(
    thread: &RuntimeThreadHandle,
    scenario: &str,
    required: &[&str],
) -> Result<(), Box<dyn Error>> {
    let names = tool_names(thread)?;
    for required_name in required {
        if !names.iter().any(|name| name == required_name) {
            return Err(fail(format!(
                "Goal Mode {scenario} did not call {required_name}: {names:?}"
            )));
        }
    }
    Ok(())
}

fn collect_audit(
    session_id: &str,
    goal_id: &GoalId,
    observer: &RecordingObserver,
) -> Result<ScenarioAudit, Box<dyn Error>> {
    let store = GoalStore::load_default()?;
    let record = store
        .get_by_session(session_id)?
        .ok_or_else(|| fail("persisted goal disappeared after the hosted turn"))?;
    if &record.goal_id != goal_id {
        return Err(fail("Goal audit loaded the wrong session identity"));
    }
    let sqlite = store.audit_snapshot(goal_id)?;
    let events = observer.events();
    let outer_turn_events = events
        .iter()
        .filter(|event| event.event_type == EventType::GoalTurnStarted)
        .count();
    let update_goal_requests = events
        .iter()
        .filter(|event| event.event_type == EventType::GoalIntentRequested)
        .count();
    let update_goal_acks = events
        .iter()
        .filter(|event| event.event_type == EventType::GoalIntentAcknowledged)
        .count();
    let rejected_acks = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::GoalIntentAcknowledged
                && event.payload["ack"]["ack"].as_str() == Some("rejected")
        })
        .count();
    let accepted_acks = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::GoalIntentAcknowledged
                && event.payload["ack"]["ack"].as_str() == Some("deferred_to_turn_end")
        })
        .count();
    let verifier_outcomes = events
        .iter()
        .filter(|event| event.event_type == EventType::GoalVerificationCompleted)
        .count();
    let continuations = events
        .iter()
        .filter(|event| event.event_type == EventType::GoalContinuationAdmitted)
        .count();
    let resume_turns = events
        .iter()
        .filter(|event| {
            event.event_type == EventType::GoalTurnStarted
                && event.payload["origin"].as_str() == Some("resume")
        })
        .count();
    let first_terminal_seq = events.iter().find_map(|event| {
        (event.event_type == EventType::GoalTransitioned
            && matches!(
                event.payload["next_state"]["status"].as_str(),
                Some("complete" | "blocked" | "paused" | "budget_limited")
            ))
        .then_some(event.seq)
    });
    let stale_continuations = first_terminal_seq
        .map(|terminal_seq| {
            events
                .iter()
                .filter(|event| {
                    event.seq > terminal_seq
                        && ((event.event_type == EventType::GoalTurnStarted
                            && event.payload["origin"].as_str() == Some("continuation"))
                            || event.event_type == EventType::GoalContinuationAdmitted)
                })
                .count()
        })
        .unwrap_or_default();
    let rejection_code = events.iter().rev().find_map(|event| {
        (event.event_type == EventType::GoalContinuationRejected)
            .then(|| event.payload["reason"].as_str().map(str::to_string))
            .flatten()
    });

    if sqlite.outer_turns != i64::try_from(outer_turn_events).unwrap_or(i64::MAX)
        || sqlite.intents != i64::try_from(accepted_acks).unwrap_or(i64::MAX)
        || update_goal_requests != update_goal_acks
        || sqlite.in_flight_runs != 0
    {
        return Err(fail(format!(
            "Goal SQLite/event audit mismatch: events_outer_turns={outer_turn_events} events_requests={update_goal_requests} events_acks={update_goal_acks} accepted_acks={accepted_acks} sqlite={sqlite:?}"
        )));
    }

    let transcript = orca_runtime::history::load_session(session_id)?;
    let journal_goal_events = transcript
        .semantic_events
        .iter()
        .filter(|event| is_goal_event(event))
        .count();
    let observed_goal_events = events.iter().filter(|event| is_goal_event(event)).count();
    if journal_goal_events != observed_goal_events {
        return Err(fail(format!(
            "Goal journal/observer mismatch: journal={journal_goal_events} observer={observed_goal_events}"
        )));
    }

    let pause_reason = match &record.state {
        GoalState::Paused { reason, .. } => Some(pause_reason_name(*reason)),
        _ => None,
    };
    Ok(ScenarioAudit {
        state: state_name(&record.state),
        reason: record
            .last_transition
            .as_ref()
            .map(|transition| transition.reason_code.clone())
            .unwrap_or_else(|| "none".to_string()),
        pause_reason,
        rejection_code,
        outer_turns: sqlite.outer_turns,
        update_goal_requests,
        update_goal_acks,
        accepted_acks,
        rejected_acks,
        persisted_intents: sqlite.intents,
        verifier_outcomes,
        verifier_tokens: sqlite.verifier_tokens,
        usage_events: sqlite.usage_events,
        charged_tokens: record.usage.charged_tokens(),
        cost_micros: record.usage.cost_micros,
        journal_goal_events,
        continuations,
        stale_continuations,
        in_flight_runs: sqlite.in_flight_runs,
        resume_turns,
    })
}

fn require_common_audit(audit: &ScenarioAudit, scenario: &str) -> Result<(), Box<dyn Error>> {
    if audit.outer_turns <= 0
        || audit.usage_events <= 0
        || audit.charged_tokens <= 0
        || audit.journal_goal_events == 0
        || audit.update_goal_requests != audit.update_goal_acks
        || audit.accepted_acks + audit.rejected_acks != audit.update_goal_acks
        || audit.continuations != 0
        || audit.stale_continuations != 0
        || audit.in_flight_runs != 0
    {
        return Err(fail(format!(
            "Goal Mode {scenario} common audit failed: {audit:?}"
        )));
    }
    Ok(())
}

fn run_completion(host: &RuntimeHost, config: RunConfig) -> Result<ScenarioAudit, Box<dyn Error>> {
    let thread = host
        .start_thread(config, "Goal real API: completion")
        .map_err(|error| fail(error.to_string()))?;
    let (session_id, goal_id) = create_goal(&thread, COMPLETION_OBJECTIVE)?;
    let observer = Arc::new(RecordingObserver::default());
    let terminal = start_goal_run(
        &thread,
        COMPLETION_PROMPT,
        observer.clone(),
        GoalTurnOrigin::User,
    )?
    .wait();
    require_outcome(terminal.outcome(), RunStatus::Success, "completion")?;
    require_tools(&thread, "completion", &["task_list", "update_goal"])?;
    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    let audit = collect_audit(&session_id, &goal_id, &observer)?;
    require_common_audit(&audit, "completion")?;
    if audit.state != "complete"
        || audit.reason != "verified_complete"
        || audit.update_goal_requests == 0
        || audit.rejected_acks != 0
        || audit.verifier_outcomes == 0
        || audit.verifier_tokens <= 0
    {
        return Err(fail(format!(
            "Goal Mode completion terminal audit failed: {audit:?}"
        )));
    }
    Ok(audit)
}

fn run_rejected_completion(
    host: &RuntimeHost,
    mut config: RunConfig,
) -> Result<ScenarioAudit, Box<dyn Error>> {
    config.approval_mode = ApprovalMode::Plan;
    let thread = host
        .start_thread(config, "Goal real API: rejected completion")
        .map_err(|error| fail(error.to_string()))?;
    let (session_id, goal_id) = create_goal(&thread, REJECTED_OBJECTIVE)?;
    let observer = Arc::new(RecordingObserver::default());
    let terminal = start_goal_run(
        &thread,
        REJECTED_PROMPT,
        observer.clone(),
        GoalTurnOrigin::User,
    )?
    .wait();
    require_outcome(
        terminal.outcome(),
        RunStatus::Success,
        "rejected_completion",
    )?;
    require_tools(&thread, "rejected_completion", &["update_goal"])?;
    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    let audit = collect_audit(&session_id, &goal_id, &observer)?;
    require_common_audit(&audit, "rejected_completion")?;
    if audit.state != "paused"
        || audit.reason != "paused"
        || audit.rejection_code.as_deref() != Some("plan_mode")
        || audit.update_goal_requests == 0
        || audit.rejected_acks == 0
        || audit.verifier_outcomes != 0
        || audit.verifier_tokens != 0
    {
        return Err(fail(format!(
            "Goal Mode rejected completion audit failed: {audit:?}"
        )));
    }
    Ok(audit)
}

fn run_blocked(host: &RuntimeHost, config: RunConfig) -> Result<ScenarioAudit, Box<dyn Error>> {
    let thread = host
        .start_thread(config, "Goal real API: blocked")
        .map_err(|error| fail(error.to_string()))?;
    let (session_id, goal_id) = create_goal(&thread, BLOCKED_OBJECTIVE)?;
    let observer = Arc::new(RecordingObserver::default());
    let terminal = start_goal_run(
        &thread,
        BLOCKED_PROMPT,
        observer.clone(),
        GoalTurnOrigin::User,
    )?
    .wait();
    require_outcome(terminal.outcome(), RunStatus::Success, "blocked")?;
    require_tools(&thread, "blocked", &["bash", "update_goal"])?;
    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    let audit = collect_audit(&session_id, &goal_id, &observer)?;
    require_common_audit(&audit, "blocked")?;
    if audit.state != "blocked"
        || audit.reason != "verified_blocked"
        || audit.update_goal_requests == 0
        || audit.rejected_acks != 0
        || audit.verifier_outcomes == 0
        || audit.verifier_tokens <= 0
    {
        return Err(fail(format!(
            "Goal Mode blocked terminal audit failed: {audit:?}"
        )));
    }
    Ok(audit)
}

fn run_cancelled_phase(
    thread: &RuntimeThreadHandle,
    observer: Arc<RecordingObserver>,
    scenario: &str,
) -> Result<(), Box<dyn Error>> {
    let operation = start_goal_run(
        thread,
        CANCELLATION_PROMPT,
        observer.clone(),
        GoalTurnOrigin::User,
    )?;
    let operation_id = operation.id();
    let control = thread.clone();
    let pause_observer = observer.clone();
    let pause = std::thread::spawn(move || {
        let saw_bash = pause_observer.wait_for_tool("bash", Duration::from_secs(60));
        let result = control.pause_goal_run(operation_id);
        (saw_bash, result)
    });
    let terminal = operation.wait();
    let (saw_bash, pause_result) = pause
        .join()
        .map_err(|_| fail("Goal cancellation control thread panicked"))?;
    if !saw_bash {
        return Err(fail(format!(
            "Goal Mode {scenario} did not reach the bash cancellation point"
        )));
    }
    if !matches!(
        pause_result.map_err(|error| fail(error.to_string()))?,
        PauseGoalRunResult::Requested { .. } | PauseGoalRunResult::AlreadyRequested { .. }
    ) {
        return Err(fail(format!("Goal Mode {scenario} pause was not accepted")));
    }
    require_outcome(terminal.outcome(), RunStatus::Cancelled, scenario)
}

fn run_cancellation(
    host: &RuntimeHost,
    config: RunConfig,
) -> Result<ScenarioAudit, Box<dyn Error>> {
    let thread = host
        .start_thread(config, "Goal real API: cancellation")
        .map_err(|error| fail(error.to_string()))?;
    let (session_id, goal_id) = create_goal(&thread, CANCELLATION_OBJECTIVE)?;
    let observer = Arc::new(RecordingObserver::default());
    run_cancelled_phase(&thread, observer.clone(), "cancellation")?;
    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    let audit = collect_audit(&session_id, &goal_id, &observer)?;
    require_common_audit(&audit, "cancellation")?;
    if audit.state != "paused"
        || audit.reason != "paused"
        || audit.pause_reason != Some("user")
        || audit.update_goal_requests != 0
        || audit.verifier_outcomes != 0
    {
        return Err(fail(format!(
            "Goal Mode cancellation audit failed: {audit:?}"
        )));
    }
    Ok(audit)
}

fn run_resume(host: &RuntimeHost, config: RunConfig) -> Result<ScenarioAudit, Box<dyn Error>> {
    let thread = host
        .start_thread(config, "Goal real API: resume")
        .map_err(|error| fail(error.to_string()))?;
    let (session_id, goal_id) = create_goal(&thread, COMPLETION_OBJECTIVE)?;
    let observer = Arc::new(RecordingObserver::default());
    run_cancelled_phase(&thread, observer.clone(), "resume setup")?;
    let runtime = thread
        .goal_runtime()
        .map_err(|error| fail(error.to_string()))?;
    runtime
        .resume(
            &session_id,
            GoalTurnOrigin::Resume,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|error| fail(error.to_string()))?;
    let terminal = start_goal_run(
        &thread,
        COMPLETION_PROMPT,
        observer.clone(),
        GoalTurnOrigin::Resume,
    )?
    .wait();
    require_outcome(terminal.outcome(), RunStatus::Success, "resume")?;
    require_tools(&thread, "resume", &["task_list", "update_goal"])?;
    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    let audit = collect_audit(&session_id, &goal_id, &observer)?;
    require_common_audit(&audit, "resume")?;
    if audit.state != "complete"
        || audit.reason != "verified_complete"
        || audit.outer_turns != 2
        || audit.resume_turns != 1
        || audit.update_goal_requests == 0
        || audit.rejected_acks != 0
        || audit.verifier_outcomes == 0
        || audit.verifier_tokens <= 0
    {
        return Err(fail(format!("Goal Mode resume audit failed: {audit:?}")));
    }
    Ok(audit)
}

fn print_audit(scenario: &str, audit: &ScenarioAudit) {
    let specific = match scenario {
        "rejected_completion" => format!(
            " rejection_code={}",
            audit.rejection_code.as_deref().unwrap_or("none")
        ),
        "cancellation" => format!(" pause_reason={}", audit.pause_reason.unwrap_or("none")),
        "resume" => format!(" resume_turns={}", audit.resume_turns),
        _ => String::new(),
    };
    println!(
        "Goal Mode real API scenario verified: scenario={scenario} state={} reason={}{} {}",
        audit.state,
        audit.reason,
        specific,
        audit.common_metrics()
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    let max_budget = parse_max_budget().map_err(fail)?;
    let api_key = load_api_key().ok_or_else(|| {
        fail("DEEPSEEK_API_KEY not found in the environment or ORCA_HOME/auth.json")
    })?;
    let config = real_api_config(api_key, max_budget).map_err(fail)?;
    let host = RuntimeHost::start().map_err(|error| fail(error.to_string()))?;

    let completion = run_completion(&host, config.clone())?;
    print_audit("completion", &completion);
    let rejected = run_rejected_completion(&host, config.clone())?;
    print_audit("rejected_completion", &rejected);
    let blocked = run_blocked(&host, config.clone())?;
    print_audit("blocked", &blocked);
    let cancellation = run_cancellation(&host, config.clone())?;
    print_audit("cancellation", &cancellation);
    let resume = run_resume(&host, config)?;
    print_audit("resume", &resume);

    host.shutdown().map_err(|error| fail(error.to_string()))?;
    let stale_continuations = completion.stale_continuations
        + rejected.stale_continuations
        + blocked.stale_continuations
        + cancellation.stale_continuations
        + resume.stale_continuations;
    let in_flight_runs = completion.in_flight_runs
        + rejected.in_flight_runs
        + blocked.in_flight_runs
        + cancellation.in_flight_runs
        + resume.in_flight_runs;
    println!(
        "Goal Mode real API e2e verified: scenarios=5 stale_continuations={stale_continuations} in_flight_runs={in_flight_runs}"
    );
    Ok(())
}
