use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
use serde::Serialize;
#[cfg(windows)]
use std::collections::BTreeMap;

use orca_platform::process::ProcessJob;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::task_types::BackgroundTaskSummary;
use orca_core::tool_types;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime, ChildAgentRuntimeContext,
};
use crate::agent_loop::execute_child_agent_loop;
use crate::hooks::HookRunner;
use crate::instructions;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::memory;
use crate::runtime_subagent_call::{append_worktree_outcome, validate_subagent_output_schema};
use crate::subagent::{self, SubagentIsolation};
use crate::tasks::TaskRegistry;
use crate::worktree::WorktreeGuard;

#[cfg(windows)]
const WINDOWS_RUNNER_PROTOCOL_VERSION: u32 = 1;

#[cfg(windows)]
#[derive(Debug, Serialize)]
struct WindowsRunnerLaunchRequest {
    version: u32,
    program: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, Option<String>>,
    job_name: Option<String>,
    forward_stdin: bool,
}

#[derive(Clone, Debug)]
pub struct AsyncSubagentWorktree {
    pub repo_root: PathBuf,
    pub path: PathBuf,
}

pub struct AsyncSubagentWorkerInput {
    pub config: RunConfig,
    pub cwd: PathBuf,
    pub child_cwd: PathBuf,
    pub task_session_id: String,
    pub agent_id: String,
    pub request: subagent::SubagentRequest,
    pub child_depth: u32,
    pub worktree: Option<AsyncSubagentWorktree>,
}

pub(crate) struct AsyncSubagentWorkerContext {
    pub input: AsyncSubagentWorkerInput,
    pub child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct AsyncSubagentLaunchContext<'a> {
    pub config: &'a RunConfig,
    pub cwd: &'a Path,
    pub tool_request: &'a tool_types::ToolRequest,
    pub request: subagent::SubagentRequest,
    pub subagent_depth: u32,
    pub task_registry: &'a TaskRegistry,
    pub root_task_id: Option<&'a str>,
}

pub(crate) struct AsyncSubagentLaunchOutput {
    pub(crate) result: tool_types::ToolResult,
    pub(crate) task: Option<BackgroundTaskSummary>,
}

struct AsyncSubagentWorkerSpawnContext<'a> {
    config: &'a RunConfig,
    cwd: &'a Path,
    child_cwd: &'a Path,
    task_session_id: &'a str,
    agent_id: &'a str,
    request: &'a subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<&'a AsyncSubagentWorktree>,
}

pub fn run_async_subagent_worker(input: AsyncSubagentWorkerInput) -> i32 {
    run_async_subagent_worker_with_executor(AsyncSubagentWorkerContext {
        input,
        child_executor: execute_child_agent_loop,
    })
}

pub(crate) fn run_async_subagent_worker_with_executor(context: AsyncSubagentWorkerContext) -> i32 {
    let AsyncSubagentWorkerContext {
        input,
        child_executor,
    } = context;
    let AsyncSubagentWorkerInput {
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
    } = input;
    let task_registry = match wait_for_async_subagent_adoption(&task_session_id, &cwd, &agent_id) {
        Ok(registry) => registry,
        Err(_) => return 1,
    };
    let lease = match task_registry.acquire_task_lease(&agent_id) {
        Ok(lease) => lease,
        Err(_) => return 1,
    };
    if task_registry
        .mark_running_with_lease(&lease, &agent_id)
        .is_err()
    {
        return 1;
    }
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let heartbeat_registry = task_registry.clone();
    let heartbeat_lease = lease.clone();
    let heartbeat_stop_signal = heartbeat_stop.clone();
    let heartbeat_agent_id = agent_id.clone();
    std::thread::spawn(move || {
        while !heartbeat_stop_signal.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_secs(5));
            if heartbeat_stop_signal.load(Ordering::Acquire) {
                break;
            }
            if heartbeat_registry
                .renew_task_lease(&heartbeat_lease, &heartbeat_agent_id)
                .is_err()
            {
                break;
            }
        }
    });
    let stop_heartbeat = || heartbeat_stop.store(true, Ordering::Release);
    let instructions = instructions::load_for_cwd_or_default(&cwd);
    let memory = memory::load_for_cwd(&cwd);
    let hooks = HookRunner::new(config.hooks.clone());
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let cancel = CancelToken::new();
    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: child_depth,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let mut child_events = EventFactory::new(format!("subagent-{agent_id}"));
    let mut child_lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"));
    child_lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut child_sink = EventSink::new(io::sink(), config.output_format);
    let (child, child_cost_tracker) = {
        let mut child_runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
            cwd: &child_cwd,
            events: &mut child_events,
            sink: &mut child_sink,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &mcp_registry,
            hooks: &hooks,
            cancel: &cancel,
            lifecycle: Some(&mut child_lifecycle),
            task_registry: Some(&task_registry),
            root_task_id: Some(&agent_id),
            executor: child_executor,
        });
        crate::agent_child::run_child_agent(&config, &child_request, &mut child_runtime)
    };
    let completed_task = child_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| {
            child_lifecycle.active_task().cloned().unwrap_or_else(|| {
                RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"))
                    .start_task(RuntimeTaskKind::Subagent)
                    .clone()
            })
        });
    let worktree = worktree.and_then(|worktree| {
        WorktreeGuard::finish_existing(worktree.repo_root, worktree.path).ok()
    });
    let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
    if child.status == RunStatus::Success {
        let mut output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        if let Err(mut error) =
            validate_subagent_output_schema(&request.description, request.schema.as_ref(), &output)
        {
            append_worktree_outcome(&mut error, worktree.as_ref());
            let failed_task = completed_task.with_status(RuntimeTaskStatus::Failed);
            let error = async_subagent_result_payload(error, Some(failed_task.payload()));
            stop_heartbeat();
            if task_registry
                .fail_with_usage_and_lease(&lease, &agent_id, error, usage)
                .is_ok()
            {
                return 1;
            }
            return 1;
        }
        append_worktree_outcome(&mut output, worktree.as_ref());
        let output = async_subagent_result_payload(output, Some(completed_task.payload()));
        stop_heartbeat();
        if task_registry
            .complete_with_usage_and_lease(&lease, &agent_id, output, usage)
            .is_ok()
        {
            return 0;
        }
    } else {
        let mut error = child
            .error
            .or(child.final_message)
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
        append_worktree_outcome(&mut error, worktree.as_ref());
        let error = async_subagent_result_payload(error, Some(completed_task.payload()));
        stop_heartbeat();
        if task_registry
            .fail_with_usage_and_lease(&lease, &agent_id, error, usage)
            .is_ok()
        {
            return 1;
        }
    }
    1
}

pub(crate) fn launch_async_subagent(
    context: AsyncSubagentLaunchContext<'_>,
) -> AsyncSubagentLaunchOutput {
    let AsyncSubagentLaunchContext {
        config,
        cwd,
        tool_request,
        request,
        subagent_depth,
        task_registry,
        root_task_id,
    } = context;
    let request = subagent::with_delegation_snapshot(
        request,
        orca_core::config::DelegationSnapshot::from_config(config),
    );
    if task_registry.is_process_local() {
        return AsyncSubagentLaunchOutput {
            result: tool_types::ToolResult::failed(
                tool_request,
                "async subagents require persistent task ownership; use sync mode for a history-disabled run",
                None,
            ),
            task: None,
        };
    }
    let agent_type = serde_json::to_value(&request.subagent_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    let task = task_registry.create_subagent_with_parent(
        request.description.clone(),
        agent_type,
        root_task_id.map(str::to_string),
    );
    let agent_id = task.id.clone();
    if task_registry.is_cancelled(&agent_id) {
        let _ = task_registry.stop(
            &agent_id,
            "Task stopped because its foreground owner was cancelled".to_string(),
        );
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::cancelled_before_start(
                tool_request,
                "the foreground operation was cancelled before the async subagent started",
            ),
        );
    }
    let worktree_guard = if request.isolation == SubagentIsolation::Worktree {
        match WorktreeGuard::create(cwd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let error = format!("failed to create subagent worktree: {error}");
                let _ = task_registry.fail(&agent_id, error.clone());
                return async_launch_output(
                    task_registry,
                    &agent_id,
                    tool_types::ToolResult::failed(tool_request, error, None),
                );
            }
        }
    } else {
        None
    };
    let child_cwd = worktree_guard
        .as_ref()
        .map(|guard| guard.path().to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let worktree = worktree_guard.as_ref().map(|guard| AsyncSubagentWorktree {
        repo_root: guard.repo_root().to_path_buf(),
        path: guard.path().to_path_buf(),
    });
    if let Err(error) = task_registry.mark_worker_spawned(&agent_id, 0) {
        let _ = task_registry.fail(&agent_id, error.clone());
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::failed(tool_request, error, None),
        );
    }
    match spawn_async_subagent_worker(AsyncSubagentWorkerSpawnContext {
        config,
        cwd,
        child_cwd: &child_cwd,
        task_session_id: task_registry.session_id(),
        agent_id: &agent_id,
        request: &request,
        child_depth: subagent_depth + 1,
        worktree: worktree.as_ref(),
    }) {
        Ok((child, process_job)) => {
            if let Err(error) =
                task_registry.adopt_subagent_worker_with_job(&agent_id, child, process_job)
            {
                let worktree = worktree_guard.and_then(|guard| guard.finish().ok());
                let mut error = format!("failed to own async subagent worker: {error}");
                append_worktree_outcome(&mut error, worktree.as_ref());
                let _ = task_registry.fail(&agent_id, error.clone());
                return async_launch_output(
                    task_registry,
                    &agent_id,
                    tool_types::ToolResult::failed(tool_request, error, None),
                );
            }
            std::mem::forget(worktree_guard);
        }
        Err(error) => {
            let worktree = worktree_guard.and_then(|guard| guard.finish().ok());
            let mut error = format!("failed to start async subagent worker: {error}");
            append_worktree_outcome(&mut error, worktree.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return async_launch_output(
                task_registry,
                &agent_id,
                tool_types::ToolResult::failed(tool_request, error, None),
            );
        }
    }

    let output = serde_json::json!({
        "status": "async_launched",
        "agent_id": agent_id,
        "description": request.description,
    })
    .to_string();
    async_launch_output(
        task_registry,
        &agent_id,
        tool_types::ToolResult::completed(tool_request, output, false),
    )
}

fn async_launch_output(
    task_registry: &TaskRegistry,
    agent_id: &str,
    result: tool_types::ToolResult,
) -> AsyncSubagentLaunchOutput {
    AsyncSubagentLaunchOutput {
        result,
        task: task_registry.summary(agent_id),
    }
}

fn wait_for_async_subagent_adoption(
    task_session_id: &str,
    cwd: &Path,
    agent_id: &str,
) -> Result<TaskRegistry, String> {
    let pid = std::process::id();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let registry = TaskRegistry::attach_for_cwd(task_session_id.to_string(), cwd);
        let record = registry.get(agent_id).ok_or_else(|| {
            format!("async subagent task '{agent_id}' disappeared before adoption")
        })?;
        if record.worker_pid == Some(pid) {
            return Ok(registry);
        }
        #[cfg(windows)]
        if record.worker_pid.is_some()
            && ProcessJob::open_named(&crate::tasks::async_worker_job_name(agent_id))
                .and_then(|job| job.contains_process(pid))
                .unwrap_or(false)
        {
            return Ok(registry);
        }
        if matches!(
            record.status,
            orca_core::task_types::TaskStatus::Stopped
                | orca_core::task_types::TaskStatus::Completed
                | orca_core::task_types::TaskStatus::Failed
                | orca_core::task_types::TaskStatus::Cancelled
        ) {
            return Err(format!(
                "async subagent task '{agent_id}' became terminal before worker adoption"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "async subagent worker was not adopted before the startup deadline"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_async_subagent_worker(
    context: AsyncSubagentWorkerSpawnContext<'_>,
) -> Result<(Child, ProcessJob), String> {
    let AsyncSubagentWorkerSpawnContext {
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
    } = context;
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let request_json = serde_json::to_string(request).map_err(|error| error.to_string())?;
    let api_key = config.api_key.as_deref();
    let mut worker_args = vec![
        "subagent-worker".to_string(),
        "--cwd".to_string(),
        cwd.to_string_lossy().into_owned(),
        "--child-cwd".to_string(),
        child_cwd.to_string_lossy().into_owned(),
        "--provider".to_string(),
        config.provider.as_str().to_string(),
        "--session-id".to_string(),
        task_session_id.to_string(),
        "--agent-id".to_string(),
        agent_id.to_string(),
        "--subagent-depth".to_string(),
        child_depth.to_string(),
        "--request-json".to_string(),
        request_json,
    ];
    if let Some(model) = config.model.as_history_value() {
        worker_args.extend(["--model".to_string(), model.to_string()]);
    }
    if api_key.is_some() {
        worker_args.push("--api-key-stdin".to_string());
    }
    if let Some(base_url) = config.base_url.as_deref() {
        worker_args.extend(["--base-url".to_string(), base_url.to_string()]);
    }
    if let Some(worktree) = worktree {
        worker_args.extend([
            "--worktree-repo-root".to_string(),
            worktree.repo_root.to_string_lossy().into_owned(),
            "--worktree-path".to_string(),
            worktree.path.to_string_lossy().into_owned(),
        ]);
    }
    #[cfg(windows)]
    {
        return spawn_async_subagent_worker_via_runner(
            &current_exe,
            worker_args,
            cwd,
            agent_id,
            api_key,
        );
    }
    #[cfg(not(windows))]
    {
        let mut command = ProcessCommand::new(current_exe);
        prepare_async_subagent_worker_command(&mut command, agent_id);
        command
            .current_dir(cwd)
            .stdin(if api_key.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(&worker_args)
            .env_remove("ORCA_API_KEY")
            .env_remove("DEEPSEEK_API_KEY");
        let (mut child, process_job) =
            ProcessJob::spawn(&mut command).map_err(|error| error.to_string())?;
        handoff_async_subagent_worker_api_key(&mut child, api_key)?;
        Ok((child, process_job))
    }
}

#[cfg(windows)]
fn spawn_async_subagent_worker_via_runner(
    current_exe: &Path,
    worker_args: Vec<String>,
    cwd: &Path,
    agent_id: &str,
    api_key: Option<&str>,
) -> Result<(Child, ProcessJob), String> {
    let executable_dir = current_exe
        .parent()
        .ok_or_else(|| "orca executable has no installation directory".to_string())?;
    let runner = executable_dir.join("orca-windows-runner.exe");
    if !runner.is_file() {
        return Err(format!(
            "Windows runner is missing beside the installed Orca executable: {}",
            runner.display()
        ));
    }
    let request = WindowsRunnerLaunchRequest {
        version: WINDOWS_RUNNER_PROTOCOL_VERSION,
        program: current_exe.to_string_lossy().into_owned(),
        args: worker_args,
        cwd: cwd.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        job_name: Some(crate::tasks::async_worker_job_name(agent_id)),
        forward_stdin: api_key.is_some(),
    };
    let mut command = ProcessCommand::new(runner);
    prepare_async_subagent_worker_command(&mut command, agent_id);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("ORCA_API_KEY")
        .env_remove("DEEPSEEK_API_KEY");
    let (mut child, process_job) =
        ProcessJob::spawn_named(&mut command, &crate::tasks::async_worker_job_name(agent_id))
            .map_err(|error| format!("failed to spawn Windows runner: {error}"))?;
    let result = child
        .stdin
        .take()
        .ok_or_else(|| "Windows runner did not expose request stdin".to_string())
        .and_then(|mut stdin| {
            serde_json::to_writer(&mut stdin, &request)
                .map_err(|error| format!("failed to encode Windows runner request: {error}"))?;
            stdin
                .write_all(b"\n")
                .map_err(|error| format!("failed to terminate Windows runner request: {error}"))?;
            if let Some(api_key) = api_key {
                stdin.write_all(api_key.as_bytes()).map_err(|error| {
                    format!("failed to hand off async subagent credential: {error}")
                })?;
            }
            Ok(())
        });
    if let Err(error) = result {
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, process_job))
}

fn prepare_async_subagent_worker_command(command: &mut ProcessCommand, agent_id: &str) {
    orca_tools::process::prepare_non_interactive_command(command);
    #[cfg(unix)]
    command.arg0(crate::tasks::subagent_worker_process_name(agent_id));
}

fn handoff_async_subagent_worker_api_key(
    child: &mut Child,
    api_key: Option<&str>,
) -> Result<(), String> {
    let Some(api_key) = api_key else {
        return Ok(());
    };
    let result = child
        .stdin
        .take()
        .ok_or_else(|| "async subagent worker did not expose credential stdin".to_string())
        .and_then(|mut stdin| {
            stdin
                .write_all(api_key.as_bytes())
                .map_err(|error| format!("failed to hand off async subagent credential: {error}"))
        });
    if result.is_err() {
        orca_tools::process::kill_child_tree(child);
        let _ = child.wait();
    }
    result
}

pub(crate) fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

pub(crate) fn async_subagent_result_payload(
    output: String,
    task: Option<serde_json::Value>,
) -> String {
    serde_json::json!({
        "output": output,
        "task": task,
    })
    .to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
        ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn process_local_registry_rejects_async_subagent_before_spawn() {
        let cwd = tempfile::tempdir().unwrap();
        let config = async_test_config(cwd.path().to_path_buf());
        let tool_request = ToolRequest {
            id: "ephemeral-async".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("inspect later".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect later",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let request = subagent::create_subagent_request(&tool_request);
        let registry = TaskRegistry::new("ephemeral-thread".to_string());

        let output = launch_async_subagent(AsyncSubagentLaunchContext {
            config: &config,
            cwd: cwd.path(),
            tool_request: &tool_request,
            request,
            subagent_depth: 0,
            task_registry: &registry,
            root_task_id: None,
        });

        assert_eq!(output.result.status, ToolStatus::Failed);
        assert!(
            output
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("use sync mode"))
        );
        assert!(output.task.is_none());
        assert!(registry.list().is_empty());
        assert!(!cwd.path().join(".orca").exists());
    }

    fn async_test_config(cwd: PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
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
        }
    }

    #[test]
    fn async_subagent_worker_command_hides_key_and_owns_process_group() {
        unsafe extern "C" {
            fn getpgid(pid: i32) -> i32;
        }

        let temp = tempfile::tempdir().unwrap();
        let key_file = temp.path().join("worker-key");
        let sentinel = "orca-secret-sentinel-not-for-argv";
        let agent_id = "task-test-worker";
        let mut command = ProcessCommand::new("sh");
        prepare_async_subagent_worker_command(&mut command, agent_id);
        command
            .env("ORCA_TEST_KEY_FILE", &key_file)
            .stdin(Stdio::piped())
            .arg("-c")
            .arg("cat > \"$ORCA_TEST_KEY_FILE\"; sleep 30");
        let mut child = command.spawn().expect("spawn worker process-group fixture");
        handoff_async_subagent_worker_api_key(&mut child, Some(sentinel))
            .expect("hand off worker credential");
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_to_string(&key_file).ok().as_deref() != Some(sentinel)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            key_file.exists(),
            "worker did not receive API key through private stdin"
        );

        let pgid = unsafe { getpgid(pid) };
        let pid_text = pid.to_string();
        let command_line = ProcessCommand::new("/bin/ps")
            .args(["-ww", "-p", pid_text.as_str(), "-o", "command="])
            .output()
            .expect("inspect worker command line");

        assert_eq!(
            pgid, pid,
            "async worker must lead an isolated process group"
        );
        let command_line = String::from_utf8_lossy(&command_line.stdout);
        assert!(
            command_line.starts_with(&crate::tasks::subagent_worker_process_name(agent_id)),
            "async worker must expose its persisted identity in argv0"
        );
        assert!(
            !command_line.contains(sentinel),
            "provider API key must not appear in worker argv"
        );
        assert!(
            !command_line.contains("--api-key"),
            "internal worker must not receive an API key argument"
        );
        assert_eq!(
            std::fs::read_to_string(&key_file).unwrap(),
            sentinel,
            "worker must receive the provider API key through its private stdin"
        );
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
    }
}
