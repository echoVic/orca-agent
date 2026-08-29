use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::SystemTime;

use orca_core::capability::CapabilitySet;
use orca_core::config::file::ConfigOverrides;
use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::workflow_types::{WorkflowInput, WorkflowRunState};
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::config::{DesktopNotifications, RunConfigRequest, build_run_config};
use crate::tasks::TaskRegistry;
use crate::workflow::script::{find_saved_workflow, parse_workflow_meta};
use crate::workflow::state::{WorkflowStateStore, read_workflow_run_state};
use crate::workflow::{WorkflowDraftStore, WorkflowLaunchRequest, WorkflowRunner};

const MAX_WORKER_API_KEY_BYTES: u64 = 64 * 1024;
const MAX_WORKFLOW_LAUNCH_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct WorkflowRunRequest {
    pub app_version: String,
    pub cwd: Option<PathBuf>,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub args: Option<String>,
    pub resume_from_run_id: Option<String>,
    pub script_or_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowListRequest {
    pub name: Option<String>,
    pub run_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkflowWorkerRequest {
    pub app_version: String,
    pub cwd: PathBuf,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_stdin: bool,
    pub base_url: Option<String>,
    pub session_id: String,
    pub input_json: String,
}

#[derive(Clone, Debug)]
pub enum WorkflowCommandRequest {
    Run(WorkflowRunRequest),
    List(WorkflowListRequest),
    Show {
        task_id: String,
    },
    Source {
        name: String,
    },
    Stop {
        task_id: String,
    },
    Pause {
        task_id: String,
    },
    Resume {
        run_id: String,
    },
    Clone {
        run_id: String,
    },
    Restart {
        run_id: String,
        phase: Option<String>,
        app_version: String,
    },
    Worker(WorkflowWorkerRequest),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowListEntry {
    task_id: String,
    run_id: String,
    session_id: String,
    workflow_name: String,
    status: orca_core::workflow_types::WorkflowRunStatus,
    cwd: String,
    total_agent_count: u32,
    final_summary: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowShowEntry {
    #[serde(flatten)]
    state: WorkflowRunState,
    session_id: String,
    run_dir: String,
    transcript_dir: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSourceEntry {
    name: String,
    path: String,
    meta: orca_core::workflow_types::WorkflowMeta,
    script: String,
}

struct PersistedWorkflowRun {
    session_id: String,
    state: WorkflowRunState,
    run_dir: PathBuf,
    state_mtime: Option<SystemTime>,
    legacy_api_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowCliLaunchRecord {
    cwd: String,
    provider: ProviderKind,
    model: Option<String>,
    base_url: Option<String>,
    capabilities: CapabilitySet,
    input: WorkflowInput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowControlResponse {
    status: &'static str,
    task_id: String,
    run_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowCloneResponse {
    status: &'static str,
    run_id: String,
    draft_id: String,
    workflow_name: String,
    script_path: String,
}

pub fn run(request: WorkflowCommandRequest) -> i32 {
    match request {
        WorkflowCommandRequest::Run(request) => run_workflow_command(request),
        WorkflowCommandRequest::List(request) => workflow_list_command(request),
        WorkflowCommandRequest::Show { task_id } => workflow_show_command(&task_id),
        WorkflowCommandRequest::Source { name } => workflow_source_command(&name),
        WorkflowCommandRequest::Stop { task_id } => workflow_stop_command(&task_id),
        WorkflowCommandRequest::Pause { task_id } => workflow_pause_command(&task_id),
        WorkflowCommandRequest::Resume { run_id } => workflow_resume_command(&run_id),
        WorkflowCommandRequest::Clone { run_id } => workflow_clone_command(&run_id),
        WorkflowCommandRequest::Restart {
            run_id,
            phase,
            app_version,
        } => workflow_restart_command(&run_id, phase, app_version),
        WorkflowCommandRequest::Worker(request) => run_workflow_worker(request),
    }
}

fn run_workflow_command(args: WorkflowRunRequest) -> i32 {
    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let run_config = match build_workflow_run_config(
        &args.app_version,
        &cwd,
        args.provider,
        args.model.clone(),
        args.api_key.clone(),
        args.base_url.clone(),
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let workflow_args = match parse_optional_json_arg(args.args.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let input = workflow_input_for_launch(
        &cwd,
        &args.script_or_name,
        workflow_args,
        args.resume_from_run_id,
    );
    if let Some(run_id) = input.resume_from_run_id.as_deref() {
        eprintln!(
            "orca: workflow resume from run '{run_id}' is only available inside the active Orca session that owns the workflow run"
        );
        return 1;
    }
    let session_id = match resolve_workflow_session_id(&cwd, input.resume_from_run_id.as_deref()) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    spawn_workflow_worker(
        &cwd,
        session_id,
        args.app_version,
        args.provider,
        args.model,
        run_config.api_key,
        args.base_url,
        run_config.workflows.capabilities.clone(),
        &input,
    )
}

fn workflow_list_command(args: WorkflowListRequest) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut runs = match load_persisted_workflow_runs(&cwd) {
        Ok(runs) => runs,
        Err(error) => {
            eprintln!("orca: failed to list workflows: {error}");
            return 1;
        }
    };
    runs.retain(|run| {
        args.name
            .as_ref()
            .is_none_or(|name| run.state.workflow_name.contains(name))
            && args
                .run_id
                .as_ref()
                .is_none_or(|run_id| run.state.run_id.contains(run_id))
            && args
                .status
                .as_ref()
                .is_none_or(|status| workflow_status_matches(run.state.status, status))
    });
    runs.sort_by(|left, right| {
        right
            .state_mtime
            .cmp(&left.state_mtime)
            .then_with(|| right.state.run_id.cmp(&left.state.run_id))
    });

    let entries = runs
        .into_iter()
        .map(|run| WorkflowListEntry {
            task_id: run.state.task_id,
            run_id: run.state.run_id,
            session_id: run.session_id,
            workflow_name: run.state.workflow_name,
            status: run.state.status,
            cwd: run.state.cwd,
            total_agent_count: run.state.total_agent_count,
            final_summary: run.state.final_summary,
            error: run.state.error,
        })
        .collect::<Vec<_>>();

    match print_json_stdout(&entries) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: failed to print workflow list: {error}");
            1
        }
    }
}

fn workflow_show_command(task_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let run = match find_workflow_by_task_id(&cwd, task_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            eprintln!("orca: workflow task '{task_id}' not found");
            return 1;
        }
        Err(error) => {
            eprintln!("orca: failed to show workflow: {error}");
            return 1;
        }
    };

    let response = WorkflowShowEntry {
        session_id: run.session_id,
        transcript_dir: run.run_dir.join("transcripts").display().to_string(),
        run_dir: run.run_dir.display().to_string(),
        state: run.state,
    };

    match print_json_stdout(&response) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: failed to print workflow details: {error}");
            1
        }
    }
}

fn workflow_source_command(name: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let user_workflow_dir = dirs::home_dir()
        .map(|home| home.join(".orca").join("workflows"))
        .unwrap_or_else(|| PathBuf::from(".orca/workflows"));
    let path = match find_saved_workflow(&cwd, name, &user_workflow_dir) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("orca: workflow source '{name}' not found: {error}");
            return 1;
        }
    };
    let script = match fs::read_to_string(&path) {
        Ok(script) => script,
        Err(error) => {
            eprintln!(
                "orca: failed to read workflow source '{}': {error}",
                path.display()
            );
            return 1;
        }
    };
    let meta = match parse_workflow_meta(&script) {
        Ok(meta) => meta,
        Err(error) => {
            eprintln!(
                "orca: failed to parse workflow source '{}': {error}",
                path.display()
            );
            return 1;
        }
    };

    match print_json_stdout(&WorkflowSourceEntry {
        name: name.to_string(),
        path: path.display().to_string(),
        meta,
        script,
    }) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: failed to print workflow source: {error}");
            1
        }
    }
}

fn workflow_stop_command(task_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_task_id(&cwd, task_id) {
        Ok(Some(run)) => {
            if !matches!(
                run.state.status,
                orca_core::workflow_types::WorkflowRunStatus::Queued
                    | orca_core::workflow_types::WorkflowRunStatus::Running
                    | orca_core::workflow_types::WorkflowRunStatus::Stopping
            ) {
                eprintln!(
                    "orca: workflow task '{}' is not active (current status: {:?})",
                    task_id, run.state.status
                );
                return 1;
            }
            let store = WorkflowStateStore::new(run.run_dir.parent().unwrap().to_path_buf());
            if let Err(error) = store.request_stop(&run.state.run_id) {
                eprintln!("orca: failed to request workflow stop: {error}");
                return 1;
            }
            match print_json_stdout(&WorkflowControlResponse {
                status: "stop_requested",
                task_id: run.state.task_id,
                run_id: run.state.run_id,
            }) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("orca: failed to print workflow stop response: {error}");
                    1
                }
            }
        }
        Ok(None) => {
            eprintln!("orca: workflow task '{task_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn workflow_pause_command(task_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_task_id(&cwd, task_id) {
        Ok(Some(run)) => {
            if !matches!(
                run.state.status,
                orca_core::workflow_types::WorkflowRunStatus::Queued
                    | orca_core::workflow_types::WorkflowRunStatus::Running
                    | orca_core::workflow_types::WorkflowRunStatus::Paused
            ) {
                eprintln!(
                    "orca: workflow task '{}' is not pausable (current status: {:?})",
                    task_id, run.state.status
                );
                return 1;
            }
            let store = WorkflowStateStore::new(run.run_dir.parent().unwrap().to_path_buf());
            if let Err(error) = store.request_pause(&run.state.run_id) {
                eprintln!("orca: failed to request workflow pause: {error}");
                return 1;
            }
            match print_json_stdout(&WorkflowControlResponse {
                status: "pause_requested",
                task_id: run.state.task_id,
                run_id: run.state.run_id,
            }) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("orca: failed to print workflow pause response: {error}");
                    1
                }
            }
        }
        Ok(None) => {
            eprintln!("orca: workflow task '{task_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn workflow_resume_command(run_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_run_id(&cwd, run_id) {
        Ok(Some(run)) => {
            let store = WorkflowStateStore::new(run.run_dir.parent().unwrap().to_path_buf());
            if let Err(error) = store.request_resume(&run.state.run_id) {
                eprintln!("orca: failed to request workflow resume: {error}");
                return 1;
            }
            match print_json_stdout(&WorkflowControlResponse {
                status: "resume_requested",
                task_id: run.state.task_id,
                run_id: run.state.run_id,
            }) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("orca: failed to print workflow resume response: {error}");
                    1
                }
            }
        }
        Ok(None) => {
            eprintln!("orca: workflow run '{run_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn workflow_clone_command(run_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_run_id(&cwd, run_id) {
        Ok(Some(run)) => {
            let runs_root = run.run_dir.parent().unwrap().to_path_buf();
            let session_dir = runs_root.parent().unwrap().to_path_buf();
            let store = WorkflowStateStore::new(runs_root);
            let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
            match draft_store.clone_from_run(&store, &run.state.run_id, 1) {
                Ok(draft) => match print_json_stdout(&WorkflowCloneResponse {
                    status: "draft_created",
                    run_id: run.state.run_id,
                    draft_id: draft.draft_id,
                    workflow_name: draft.name,
                    script_path: draft.script_path,
                }) {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("orca: failed to print workflow clone response: {error}");
                        1
                    }
                },
                Err(error) => {
                    eprintln!("orca: failed to clone workflow run: {error}");
                    1
                }
            }
        }
        Ok(None) => {
            eprintln!("orca: workflow run '{run_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn workflow_restart_command(
    run_id: &str,
    restart_phase: Option<String>,
    app_version: String,
) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_run_id_for_restart(&cwd, run_id) {
        Ok(Some(run)) => {
            let record = match read_workflow_cli_launch_record(&run.run_dir) {
                Ok(record) => record,
                Err(error) => {
                    eprintln!("orca: failed to read workflow launch record: {error}");
                    return 1;
                }
            };
            let launch_cwd = PathBuf::from(&record.cwd);
            let mut input = record.input;
            input.resume_from_run_id = Some(run.state.run_id.clone());
            input.restart_phase = restart_phase;
            spawn_workflow_worker(
                &launch_cwd,
                run.session_id,
                app_version,
                record.provider,
                record.model,
                run.legacy_api_key,
                record.base_url,
                record.capabilities,
                &input,
            )
        }
        Ok(None) => {
            eprintln!("orca: workflow run '{run_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn run_workflow_worker(args: WorkflowWorkerRequest) -> i32 {
    let worker_api_key = match resolve_worker_api_key(args.api_key, args.api_key_stdin) {
        Ok(api_key) => api_key,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let input: WorkflowInput = match serde_json::from_str(&args.input_json) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("orca: invalid workflow worker input JSON: {error}");
            return 1;
        }
    };
    let config = match build_workflow_run_config(
        &args.app_version,
        &args.cwd,
        args.provider,
        args.model.clone(),
        worker_api_key,
        args.base_url.clone(),
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let workflow_capabilities = config.workflows.capabilities.clone();
    let session_dir = workflow_session_root(&args.cwd).join(&args.session_id);
    let tasks = TaskRegistry::new(args.session_id.clone());
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launch = match runner.launch_background(WorkflowLaunchRequest::from(input.clone())) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("orca: failed to launch workflow: {error}");
            return 1;
        }
    };

    let run_dir = session_dir.join("workflow-runs").join(&launch.run_id);
    if let Err(error) = write_workflow_cli_launch_record(
        &run_dir,
        &WorkflowCliLaunchRecord {
            cwd: args.cwd.display().to_string(),
            provider: args.provider,
            model: args.model,
            base_url: args.base_url,
            capabilities: workflow_capabilities,
            input,
        },
    ) {
        eprintln!("orca: failed to persist workflow launch record: {error}");
        return 1;
    }

    if let Err(error) = print_json_stdout(&launch.output) {
        eprintln!("orca: failed to write workflow output: {error}");
        return 1;
    }

    match launch.join() {
        Ok(Ok(_)) => 0,
        Ok(Err(_)) => 1,
        Err(_) => 1,
    }
}

fn build_workflow_run_config(
    app_version: &str,
    cwd: &Path,
    provider: ProviderKind,
    model_override: Option<String>,
    api_key_override: Option<String>,
    base_url_override: Option<String>,
) -> Result<orca_core::config::RunConfig, String> {
    let mut request = RunConfigRequest::new(app_version, cwd.to_path_buf());
    request.runtime_cwd = Some(cwd.to_path_buf());
    request.output_format = OutputFormat::Jsonl;
    request.provider = provider;
    request.history_mode = HistoryMode::Disabled;
    request.desktop_notifications = DesktopNotifications::Disabled;
    request.overrides = ConfigOverrides {
        model: model_override,
        mode: None,
        api_key: api_key_override,
        base_url: base_url_override,
        reasoning_effort: None,
    };
    let config = build_run_config(request)?;
    if !config.workflows.enabled {
        return Err("workflows are disabled".to_string());
    }
    Ok(config)
}

fn parse_optional_json_arg(raw: Option<&str>) -> Result<Option<Value>, String> {
    match raw {
        Some(raw) => serde_json::from_str(raw)
            .map(Some)
            .map_err(|error| format!("invalid JSON for --args: {error}")),
        None => Ok(None),
    }
}

fn spawn_workflow_worker(
    cwd: &Path,
    session_id: String,
    app_version: String,
    provider: ProviderKind,
    model: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    capabilities: CapabilitySet,
    input: &WorkflowInput,
) -> i32 {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("orca: failed to resolve current executable: {error}");
            return 1;
        }
    };
    let input_json = match serde_json::to_string(input) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("orca: failed to encode workflow input: {error}");
            return 1;
        }
    };

    let mut command = ProcessCommand::new(current_exe);
    let has_api_key = api_key.is_some();
    command
        .current_dir(cwd)
        .stdin(if has_api_key {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .arg("workflow")
        .arg("worker")
        .arg("--cwd")
        .arg(cwd)
        .arg("--app-version")
        .arg(app_version)
        .arg("--provider")
        .arg(provider.as_str())
        .arg("--session-id")
        .arg(&session_id)
        .arg("--input-json")
        .arg(input_json)
        .env_remove("ORCA_API_KEY")
        .env_remove("DEEPSEEK_API_KEY");
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    if has_api_key {
        command.arg("--api-key-stdin");
    }
    if let Some(base_url) = base_url {
        command.arg("--base-url").arg(base_url);
    }
    if let Err(error) = orca_platform::process::clear_current_process_std_handle_inheritance() {
        eprintln!("orca: failed to isolate workflow worker standard handles: {error}");
        return 1;
    }

    let cwd_for_broker = cwd.to_path_buf();
    let broker = ExecutionBroker::with_backend(
        orca_core::capability::EnforcementState::Advisory,
        "workflow-worker-user-trusted",
    );
    let launched = match broker.launch_user_trusted(
        command,
        format!("workflow-worker:{session_id}"),
        cwd_for_broker,
        capabilities,
    ) {
        Ok(launched) => launched,
        Err(LaunchError::Spawn(error)) => {
            eprintln!("orca: failed to start workflow worker: {error}");
            return 1;
        }
        Err(error) => {
            eprintln!("orca: workflow worker broker rejected launch: {error:?}");
            return 1;
        }
    };
    let mut child = launched.child;
    if let Err(error) = handoff_workflow_worker_api_key(&mut child, api_key.as_deref()) {
        eprintln!("orca: {error}");
        return 1;
    }

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            eprintln!("orca: workflow worker did not expose stdout");
            return 1;
        }
    };
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    match reader.read_line(&mut first_line) {
        Ok(0) => {
            let _ = child.wait();
            eprintln!("orca: workflow worker exited before reporting launch output");
            1
        }
        Ok(_) => {
            print!("{}", first_line);
            0
        }
        Err(error) => {
            let _ = child.wait();
            eprintln!("orca: failed to read workflow worker launch output: {error}");
            1
        }
    }
}

fn handoff_workflow_worker_api_key(
    child: &mut std::process::Child,
    api_key: Option<&str>,
) -> Result<(), String> {
    let Some(api_key) = api_key else {
        return Ok(());
    };
    let result = child
        .stdin
        .take()
        .ok_or_else(|| "workflow worker did not expose credential stdin".to_string())
        .and_then(|mut stdin| {
            stdin
                .write_all(api_key.as_bytes())
                .map_err(|error| format!("failed to hand off workflow credential: {error}"))
        });
    if result.is_err() {
        orca_tools::process::kill_child_tree(child);
        let _ = child.wait();
    }
    result
}

fn resolve_worker_api_key(
    api_key_arg: Option<String>,
    api_key_stdin: bool,
) -> Result<Option<String>, String> {
    resolve_worker_api_key_from_reader(api_key_arg, api_key_stdin, std::io::stdin())
}

fn resolve_worker_api_key_from_reader(
    api_key_arg: Option<String>,
    api_key_stdin: bool,
    reader: impl Read,
) -> Result<Option<String>, String> {
    if !api_key_stdin {
        return Ok(api_key_arg);
    }
    if api_key_arg.is_some() {
        return Err("--api-key and --api-key-stdin cannot be used together".to_string());
    }
    let mut api_key = String::new();
    reader
        .take(MAX_WORKER_API_KEY_BYTES + 1)
        .read_to_string(&mut api_key)
        .map_err(|error| format!("failed to read worker credential from stdin: {error}"))?;
    if api_key.len() as u64 > MAX_WORKER_API_KEY_BYTES {
        return Err("worker credential from stdin exceeds 64 KiB".to_string());
    }
    Ok(Some(api_key))
}

fn workflow_input_for_launch(
    cwd: &Path,
    script_or_name: &str,
    args: Option<Value>,
    resume_from_run_id: Option<String>,
) -> WorkflowInput {
    let script_path = PathBuf::from(script_or_name);
    WorkflowInput {
        draft_id: None,
        script_path: if script_path.is_absolute() || cwd.join(script_or_name).exists() {
            Some(script_or_name.to_string())
        } else {
            None
        },
        name: if script_path.is_absolute() || cwd.join(script_or_name).exists() {
            None
        } else {
            Some(script_or_name.to_string())
        },
        description: None,
        title: None,
        script: None,
        args,
        token_budget: None,
        resume_from_run_id,
        restart_phase: None,
    }
}

fn workflow_session_root(cwd: &Path) -> PathBuf {
    cwd.join(".orca").join("workflow-sessions")
}

fn workflow_status_matches(
    status: orca_core::workflow_types::WorkflowRunStatus,
    expected: &str,
) -> bool {
    let label = match status {
        orca_core::workflow_types::WorkflowRunStatus::Queued => "queued",
        orca_core::workflow_types::WorkflowRunStatus::Running => "running",
        orca_core::workflow_types::WorkflowRunStatus::Paused => "paused",
        orca_core::workflow_types::WorkflowRunStatus::Stopping => "stopping",
        orca_core::workflow_types::WorkflowRunStatus::Stopped => "stopped",
        orca_core::workflow_types::WorkflowRunStatus::Completed => "completed",
        orca_core::workflow_types::WorkflowRunStatus::Failed => "failed",
        orca_core::workflow_types::WorkflowRunStatus::Cancelled => "cancelled",
        orca_core::workflow_types::WorkflowRunStatus::AsyncLaunched => "async_launched",
    };
    label == expected.trim()
}

fn resolve_workflow_session_id(
    cwd: &Path,
    resume_from_run_id: Option<&str>,
) -> Result<String, String> {
    match resume_from_run_id {
        Some(run_id) => find_workflow_by_run_id(cwd, run_id)?
            .map(|run| run.session_id)
            .ok_or_else(|| format!("workflow run '{run_id}' not found")),
        None => Ok(format!("workflow-cli-{}", uuid::Uuid::new_v4())),
    }
}

fn load_persisted_workflow_runs(cwd: &Path) -> Result<Vec<PersistedWorkflowRun>, String> {
    load_persisted_workflow_runs_inner(cwd, None)
}

fn load_persisted_workflow_runs_inner(
    cwd: &Path,
    capture_legacy_key_for_run_id: Option<&str>,
) -> Result<Vec<PersistedWorkflowRun>, String> {
    let root = workflow_session_root(cwd);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut runs = Vec::new();
    for session_entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let session_entry = session_entry.map_err(|error| error.to_string())?;
        if !session_entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let session_id = session_entry.file_name().to_string_lossy().to_string();
        let runs_dir = session_entry.path().join("workflow-runs");
        if !runs_dir.exists() {
            continue;
        }
        for run_entry in fs::read_dir(&runs_dir).map_err(|error| error.to_string())? {
            let run_entry = run_entry.map_err(|error| error.to_string())?;
            if !run_entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let migrated_api_key = migrate_legacy_workflow_cli_launch_record(&run_entry.path())?;
            let state_path = run_entry.path().join("state.json");
            let state = match read_workflow_run_state(&state_path) {
                Ok(state) => state,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    return Err(format!(
                        "invalid workflow state at {}: {error}",
                        state_path.display()
                    ));
                }
                Err(error) => return Err(error.to_string()),
            };
            let legacy_api_key = capture_legacy_key_for_run_id
                .is_some_and(|run_id| run_id == state.run_id)
                .then_some(migrated_api_key)
                .flatten();
            let state_mtime = fs::metadata(&state_path)
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            runs.push(PersistedWorkflowRun {
                session_id: session_id.clone(),
                state,
                run_dir: run_entry.path(),
                state_mtime,
                legacy_api_key,
            });
        }
    }

    Ok(runs)
}

fn find_workflow_by_task_id(
    cwd: &Path,
    task_id: &str,
) -> Result<Option<PersistedWorkflowRun>, String> {
    Ok(load_persisted_workflow_runs(cwd)?
        .into_iter()
        .find(|run| run.state.task_id == task_id))
}

fn find_workflow_by_run_id(
    cwd: &Path,
    run_id: &str,
) -> Result<Option<PersistedWorkflowRun>, String> {
    Ok(load_persisted_workflow_runs(cwd)?
        .into_iter()
        .find(|run| run.state.run_id == run_id))
}

fn find_workflow_by_run_id_for_restart(
    cwd: &Path,
    run_id: &str,
) -> Result<Option<PersistedWorkflowRun>, String> {
    Ok(load_persisted_workflow_runs_inner(cwd, Some(run_id))?
        .into_iter()
        .find(|run| run.state.run_id == run_id))
}

fn print_json_stdout(value: &impl Serialize) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|error| error.to_string())?;
    stdout.write_all(b"\n").map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())
}

fn write_workflow_cli_launch_record(
    run_dir: &Path,
    record: &WorkflowCliLaunchRecord,
) -> Result<(), String> {
    fs::create_dir_all(run_dir).map_err(|error| error.to_string())?;
    let path = workflow_cli_launch_record_path(run_dir);
    let content = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    fs::write(path, content).map_err(|error| error.to_string())
}

fn read_workflow_cli_launch_record(run_dir: &Path) -> Result<WorkflowCliLaunchRecord, String> {
    let path = workflow_cli_launch_record_path(run_dir);
    let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content).map_err(|error| {
        format!(
            "invalid workflow launch record at {}: {error}",
            path.display()
        )
    })
}

fn migrate_legacy_workflow_cli_launch_record(run_dir: &Path) -> Result<Option<String>, String> {
    let path = workflow_cli_launch_record_path(run_dir);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect workflow launch record at {}: {error}",
                path.display()
            ));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "workflow launch record at {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() > MAX_WORKFLOW_LAUNCH_RECORD_BYTES {
        return Err(format!(
            "workflow launch record at {} exceeds {} bytes",
            path.display(),
            MAX_WORKFLOW_LAUNCH_RECORD_BYTES
        ));
    }

    let mut open_options = fs::OpenOptions::new();
    open_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        open_options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = open_options.open(&path).map_err(|error| {
        format!(
            "failed to read workflow launch record at {}: {error}",
            path.display()
        )
    })?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect workflow launch record at {}: {error}",
            path.display()
        )
    })?;
    if !opened_metadata.is_file() {
        return Err(format!(
            "workflow launch record at {} is not a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.dev() != opened_metadata.dev() || metadata.ino() != opened_metadata.ino() {
            return Err(format!(
                "workflow launch record at {} changed while it was opened",
                path.display()
            ));
        }
    }
    let mut content =
        Vec::with_capacity(opened_metadata.len().min(MAX_WORKFLOW_LAUNCH_RECORD_BYTES) as usize);
    Read::by_ref(&mut file)
        .take(MAX_WORKFLOW_LAUNCH_RECORD_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|error| {
            format!(
                "failed to read workflow launch record at {}: {error}",
                path.display()
            )
        })?;
    if content.len() as u64 > MAX_WORKFLOW_LAUNCH_RECORD_BYTES {
        return Err(format!(
            "workflow launch record at {} exceeds {} bytes",
            path.display(),
            MAX_WORKFLOW_LAUNCH_RECORD_BYTES
        ));
    }
    let mut value: Value = serde_json::from_slice(&content).map_err(|error| {
        format!(
            "invalid workflow launch record at {}: {error}",
            path.display()
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        format!(
            "invalid workflow launch record at {}: expected a JSON object",
            path.display()
        )
    })?;
    let camel_case_key = object.remove("apiKey");
    let snake_case_key = object.remove("api_key");
    if camel_case_key.is_none() && snake_case_key.is_none() {
        return Ok(None);
    }
    let legacy_api_key = camel_case_key
        .as_ref()
        .and_then(Value::as_str)
        .or_else(|| snake_case_key.as_ref().and_then(Value::as_str))
        .map(ToString::to_string);

    let replacement = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    if let Err(error) = atomic_write(&path, &replacement, AtomicWritePolicy::NoFollow) {
        return Err(format!(
            "failed to sanitize workflow launch record at {}: {error}",
            path.display()
        ));
    }

    Ok(legacy_api_key)
}

fn workflow_cli_launch_record_path(run_dir: &Path) -> PathBuf {
    run_dir.join("cli-launch.json")
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[cfg(windows)]
    #[test]
    fn workflow_run_scan_waits_for_a_transient_atomic_replace_gap() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = workflow_session_root(temp.path())
            .join("session-retry")
            .join("workflow-runs")
            .join("run-retry");
        fs::create_dir_all(&run_dir).unwrap();
        let path = run_dir.join("state.json");
        let state = serde_json::to_string(&WorkflowRunState {
            run_id: "run-retry".to_string(),
            task_id: "task-retry".to_string(),
            session_id: "session-retry".to_string(),
            cwd: "/tmp/workspace".to_string(),
            workflow_name: "retry".to_string(),
            meta: orca_core::workflow_types::WorkflowMeta {
                name: "retry".to_string(),
                description: "retry a transient read".to_string(),
                phases: Vec::new(),
                tags: Vec::new(),
                version: None,
            },
            script_digest: "script".to_string(),
            args_digest: "args".to_string(),
            status: orca_core::workflow_types::WorkflowRunStatus::Running,
            phases: Vec::new(),
            total_agent_count: 0,
            final_summary: None,
            error: None,
        })
        .unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(writer_path, state).unwrap();
        });

        let runs = load_persisted_workflow_runs(temp.path()).unwrap();
        writer.join().unwrap();

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state.task_id, "task-retry");
    }

    #[test]
    fn workflow_launch_record_never_serializes_api_key() {
        let temp = tempfile::tempdir().unwrap();
        let record = WorkflowCliLaunchRecord {
            cwd: "/tmp/workspace".to_string(),
            provider: ProviderKind::DeepSeek,
            model: Some("deepseek-v4-pro".to_string()),
            base_url: None,
            capabilities: CapabilitySet::read_only(),
            input: workflow_input_for_launch(Path::new("/tmp/workspace"), "workflow", None, None),
        };

        let json = serde_json::to_string(&record).unwrap();
        write_workflow_cli_launch_record(temp.path(), &record).unwrap();
        let persisted = fs::read_to_string(workflow_cli_launch_record_path(temp.path())).unwrap();

        assert!(!json.contains("apiKey"));
        assert!(!json.contains("api_key"));
        assert!(!persisted.contains("apiKey"));
        assert!(!persisted.contains("api_key"));
        read_workflow_cli_launch_record(temp.path()).unwrap();
    }

    #[test]
    fn legacy_workflow_launch_record_api_key_is_ignored_by_typed_reader() {
        let record: WorkflowCliLaunchRecord = serde_json::from_value(serde_json::json!({
            "cwd": "/tmp/workspace",
            "provider": "deep-seek",
            "model": null,
            "apiKey": "legacy-secret",
            "baseUrl": null,
            "capabilities": {"read": true, "write": false, "metadata_write": false, "network": false, "shell": false, "agent": false},
            "input": { "name": "workflow" }
        }))
        .unwrap();

        assert_eq!(record.cwd, "/tmp/workspace");
    }

    #[test]
    fn workflow_launch_record_migration_rejects_oversized_files() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("oversized");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            workflow_cli_launch_record_path(&run_dir),
            vec![b'x'; MAX_WORKFLOW_LAUNCH_RECORD_BYTES as usize + 1],
        )
        .unwrap();

        let error = migrate_legacy_workflow_cli_launch_record(&run_dir).unwrap_err();

        assert!(error.contains("exceeds"));
    }

    #[test]
    #[cfg(unix)]
    fn workflow_launch_record_migration_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("symlinked");
        let target = temp.path().join("target.json");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(&target, r#"{"apiKey":"must-not-change"}"#).unwrap();
        symlink(&target, workflow_cli_launch_record_path(&run_dir)).unwrap();

        let error = migrate_legacy_workflow_cli_launch_record(&run_dir).unwrap_err();

        assert!(error.contains("not a regular file"));
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            r#"{"apiKey":"must-not-change"}"#
        );
    }

    #[test]
    #[cfg(unix)]
    fn workflow_launch_record_migration_is_atomic_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("run-a");
        fs::create_dir_all(&run_dir).unwrap();
        let path = workflow_cli_launch_record_path(&run_dir);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "cwd": temp.path(),
                "provider": "deepseek",
                "model": null,
                "apiKey": "legacy-first",
                "baseUrl": null,
                "input": { "name": "workflow-a" },
                "futureField": { "preserved": true }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let key = migrate_legacy_workflow_cli_launch_record(&run_dir).unwrap();

        assert_eq!(key.as_deref(), Some("legacy-first"));
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(value.get("apiKey").is_none());
        assert_eq!(value["futureField"]["preserved"], true);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::read_dir(&run_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    #[cfg(unix)]
    fn workflow_worker_receives_key_without_exposing_it_in_argv() {
        let sentinel = "orca-secret-sentinel-workflow-argv";
        let temp = tempfile::tempdir().unwrap();
        let key_file = temp.path().join("key");
        let mut command = ProcessCommand::new("sh");
        orca_tools::process::prepare_non_interactive_command(&mut command);
        command
            .env("ORCA_TEST_KEY_FILE", &key_file)
            .stdin(Stdio::piped())
            .arg("-c")
            .arg("cat > \"$ORCA_TEST_KEY_FILE\"; sleep 30");
        let mut child = command.spawn().unwrap();

        handoff_workflow_worker_api_key(&mut child, Some(sentinel)).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while fs::read_to_string(&key_file).ok().as_deref() != Some(sentinel)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(key_file.exists(), "workflow worker fixture did not start");
        let pid = child.id().to_string();
        let output = ProcessCommand::new("/bin/ps")
            .args(["-ww", "-p", pid.as_str(), "-o", "command="])
            .output()
            .unwrap();
        let command_line = String::from_utf8_lossy(&output.stdout);
        assert!(!command_line.contains(sentinel));
        assert!(!command_line.contains("--api-key"));
        assert_eq!(fs::read_to_string(&key_file).unwrap(), sentinel);
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
    }

    #[test]
    fn worker_key_stdin_handoff_is_bounded_and_exclusive() {
        assert_eq!(
            resolve_worker_api_key_from_reader(None, true, io::Cursor::new(b"private-key"))
                .unwrap(),
            Some("private-key".to_string())
        );
        assert!(
            resolve_worker_api_key_from_reader(Some("key".to_string()), true, io::empty()).is_err()
        );
        let oversized = vec![b'x'; 64 * 1024 + 1];
        assert!(
            resolve_worker_api_key_from_reader(None, true, io::Cursor::new(oversized)).is_err()
        );
    }
}
