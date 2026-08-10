use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{OutputFormat, ProviderKind};

#[derive(Debug, Parser)]
#[command(name = "orca")]
#[command(version)]
#[command(about = "A DeepSeek-native coding agent.")]
pub struct Cli {
    /// Resume a saved conversation in TUI mode; omit the selector to open the picker.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Fork a saved conversation in TUI mode by ID, prefix, or 'latest'.
    #[arg(long, alias = "fork-session")]
    fork: Option<String>,

    /// Continue the latest saved conversation in TUI mode.
    #[arg(long = "continue", alias = "last")]
    continue_latest: bool,

    /// Show the TUI session picker at startup.
    #[arg(long, hide = true)]
    session_picker: bool,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// Approval mode to use, 'server' for stdin/stdout JSON-RPC mode, or 'acp' for Agent Client Protocol mode.
    #[arg(long = "mode", alias = "approval-mode")]
    mode: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run in the default interactive placeholder.
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a task and emit events.
    Exec(ExecArgs),
    /// Run and inspect local workflows.
    Workflow(WorkflowArgs),
    /// Inspect or update folder trust.
    Trust(TrustArgs),
    /// Execute a persisted async subagent task.
    #[command(hide = true)]
    SubagentWorker(SubagentWorkerArgs),
}

#[derive(Debug, Parser)]
#[command(
    override_usage = "orca exec [OPTIONS] [PROMPT]...\n       orca exec [OPTIONS] <COMMAND> [ARGS]"
)]
struct ExecArgs {
    /// Resume a saved conversation as a non-interactive session.
    #[command(subcommand)]
    command: Option<ExecCommand>,

    /// Output format: text (human-readable) or jsonl (machine-readable).
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Text, global = true)]
    output_format: OutputFormatArg,

    /// Workspace directory.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,

    /// Approval policy for tool actions.
    #[arg(long = "mode", alias = "approval-mode", value_enum, global = true)]
    approval_mode: Option<ApprovalMode>,

    /// Model to use (overrides config file and DEEPSEEK_MODEL env).
    #[arg(long, global = true)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long, global = true)]
    api_key: Option<String>,

    /// API base URL (overrides config file and DEEPSEEK_BASE_URL env).
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Optional verifier command to run after completion.
    #[arg(long, global = true)]
    verifier: Option<String>,

    /// Maximum estimated USD budget for this run.
    #[arg(long, global = true)]
    max_budget: Option<f64>,

    /// Resume a saved conversation by ID, prefix, or 'latest'.
    #[arg(long)]
    resume: Option<String>,

    /// Restore the resumed conversation only up to this persisted message id.
    #[arg(long = "resume-at", value_name = "MESSAGE_ID")]
    resume_at: Option<String>,

    /// Fork a saved conversation by ID, prefix, or 'latest'.
    #[arg(long, alias = "fork-session")]
    fork: Option<String>,

    /// Continue from the latest saved conversation.
    #[arg(long = "continue", alias = "last")]
    continue_latest: bool,

    /// Do not write this run to local history.
    #[arg(long)]
    no_history: bool,

    /// Write local history even when using machine-readable jsonl output.
    #[arg(long)]
    save_history: bool,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true, global = true)]
    provider: ProviderKind,

    /// Prompt to execute.
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum ExecCommand {
    /// Resume a saved conversation by ID, prefix, or 'latest'.
    Resume(ExecResumeArgs),
}

#[derive(Debug, Parser)]
struct ExecResumeArgs {
    /// Session id, prefix, or 'latest' to resume. Omit with --last to pick the most recent.
    #[arg(value_name = "SESSION_ID", required_unless_present = "last")]
    session_id: Option<String>,

    /// Continue the most recent recorded session.
    #[arg(long)]
    last: bool,

    /// Restore the resumed conversation only up to this persisted message id.
    #[arg(long = "resume-at", value_name = "MESSAGE_ID")]
    resume_at: Option<String>,

    /// Prompt to execute.
    prompt: Vec<String>,
}

#[derive(Debug, Parser)]
struct WorkflowArgs {
    #[command(subcommand)]
    command: WorkflowCommand,
}

#[derive(Debug, Parser)]
struct TrustArgs {
    /// Trust action.
    #[arg(value_enum, default_value_t = TrustAction::Show)]
    action: TrustAction,

    /// Folder to inspect or update.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TrustAction {
    Show,
    Add,
    Remove,
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Launch a workflow script or named workflow.
    Run(WorkflowRunArgs),
    /// List persisted workflow runs for the current project.
    List(WorkflowListArgs),
    /// Show a persisted workflow run by task id.
    Show { task_id: String },
    /// Show a saved workflow source by name.
    Source { name: String },
    /// Request stop for a workflow task.
    Stop { task_id: String },
    /// Request pause for a workflow task.
    Pause { task_id: String },
    /// Resume a paused workflow run.
    Resume { run_id: String },
    /// Clone a persisted workflow run as an editable draft.
    Clone { run_id: String },
    /// Restart failed agents from a persisted workflow run.
    RestartFailed { run_id: String },
    /// Restart one workflow phase while reusing cached results from other phases.
    RestartPhase { run_id: String, phase: String },
    #[command(hide = true)]
    Worker(WorkflowWorkerArgs),
}

#[derive(Debug, Default, Parser)]
struct WorkflowListArgs {
    /// Filter by workflow name.
    #[arg(long)]
    name: Option<String>,

    /// Filter by workflow run id.
    #[arg(long = "run-id")]
    run_id: Option<String>,

    /// Filter by workflow status, such as running, failed, or completed.
    #[arg(long)]
    status: Option<String>,
}

#[derive(Debug, Parser)]
struct WorkflowRunArgs {
    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Workflow arguments as JSON.
    #[arg(long)]
    args: Option<String>,

    /// Resume cached agent calls from a prior workflow run id.
    #[arg(long = "resume-from-run-id")]
    resume_from_run_id: Option<String>,

    /// Workflow script path or named workflow.
    script_or_name: String,
}

#[derive(Debug, Parser)]
struct WorkflowWorkerArgs {
    /// Product version inherited from the parent executable.
    #[arg(long, hide = true, default_value = env!("CARGO_PKG_VERSION"))]
    app_version: String,

    /// Workspace directory.
    #[arg(long)]
    cwd: PathBuf,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// Read the API key once from stdin (internal worker handoff).
    #[arg(long, hide = true)]
    api_key_stdin: bool,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Persisted workflow session identifier.
    #[arg(long)]
    session_id: String,

    /// Full workflow input payload as JSON.
    #[arg(long)]
    input_json: String,
}

impl From<WorkflowArgs> for orca_runtime::workflow::command::WorkflowCommandRequest {
    fn from(args: WorkflowArgs) -> Self {
        use orca_runtime::workflow::command::{
            WorkflowCommandRequest, WorkflowListRequest, WorkflowRunRequest, WorkflowWorkerRequest,
        };

        match args.command {
            WorkflowCommand::Run(args) => WorkflowCommandRequest::Run(WorkflowRunRequest {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                cwd: args.cwd,
                provider: args.provider,
                model: args.model,
                api_key: args.api_key,
                base_url: args.base_url,
                args: args.args,
                resume_from_run_id: args.resume_from_run_id,
                script_or_name: args.script_or_name,
            }),
            WorkflowCommand::List(args) => WorkflowCommandRequest::List(WorkflowListRequest {
                name: args.name,
                run_id: args.run_id,
                status: args.status,
            }),
            WorkflowCommand::Show { task_id } => WorkflowCommandRequest::Show { task_id },
            WorkflowCommand::Source { name } => WorkflowCommandRequest::Source { name },
            WorkflowCommand::Stop { task_id } => WorkflowCommandRequest::Stop { task_id },
            WorkflowCommand::Pause { task_id } => WorkflowCommandRequest::Pause { task_id },
            WorkflowCommand::Resume { run_id } => WorkflowCommandRequest::Resume { run_id },
            WorkflowCommand::Clone { run_id } => WorkflowCommandRequest::Clone { run_id },
            WorkflowCommand::RestartFailed { run_id } => WorkflowCommandRequest::Restart {
                run_id,
                phase: None,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            WorkflowCommand::RestartPhase { run_id, phase } => WorkflowCommandRequest::Restart {
                run_id,
                phase: Some(phase),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            WorkflowCommand::Worker(args) => {
                WorkflowCommandRequest::Worker(WorkflowWorkerRequest {
                    app_version: args.app_version,
                    cwd: args.cwd,
                    provider: args.provider,
                    model: args.model,
                    api_key: args.api_key,
                    api_key_stdin: args.api_key_stdin,
                    base_url: args.base_url,
                    session_id: args.session_id,
                    input_json: args.input_json,
                })
            }
        }
    }
}

#[derive(Debug, Parser)]
struct SubagentWorkerArgs {
    /// Product version inherited from the parent executable.
    #[arg(long, hide = true, default_value = env!("CARGO_PKG_VERSION"))]
    app_version: String,

    /// Workspace directory where the parent async task was launched.
    #[arg(long)]
    cwd: PathBuf,

    /// Workspace directory where the child agent should execute.
    #[arg(long)]
    child_cwd: PathBuf,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// Read the API key once from stdin (internal worker handoff).
    #[arg(long, hide = true)]
    api_key_stdin: bool,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Persisted task session identifier.
    #[arg(long)]
    session_id: String,

    /// Persisted async subagent task identifier.
    #[arg(long)]
    agent_id: String,

    /// Child subagent depth.
    #[arg(long)]
    subagent_depth: u32,

    /// Full subagent request payload as JSON.
    #[arg(long)]
    request_json: String,

    /// Parent git repository root for isolated worktree cleanup.
    #[arg(long)]
    worktree_repo_root: Option<PathBuf>,

    /// Child git worktree path for isolated worktree cleanup.
    #[arg(long)]
    worktree_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormatArg {
    Jsonl,
    Text,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(value: OutputFormatArg) -> Self {
        match value {
            OutputFormatArg::Jsonl => OutputFormat::Jsonl,
            OutputFormatArg::Text => OutputFormat::Text,
        }
    }
}

impl ExecArgs {
    fn into_request(self) -> Result<orca_runtime::command::exec::ExecCommandRequest, String> {
        let (resume, continue_latest, resume_at, prompt) = match self.command {
            Some(ExecCommand::Resume(resume_args)) => {
                if self.resume.is_some() || self.fork.is_some() || self.continue_latest {
                    return Err(
                        "the 'resume' subcommand cannot be combined with --resume/--fork/--continue"
                            .to_string(),
                    );
                }
                if self.no_history {
                    return Err(
                        "the 'resume' subcommand cannot be combined with --no-history".to_string(),
                    );
                }
                // When --last is used without an explicit prompt, clap cannot
                // express the conditional positional meaning, so the first
                // positional is reinterpreted as the prompt (Codex-style).
                let (selector, prompt) = if resume_args.last && resume_args.prompt.is_empty() {
                    (None, resume_args.session_id.into_iter().collect())
                } else {
                    (resume_args.session_id, resume_args.prompt)
                };
                let selector = selector.or_else(|| resume_args.last.then(|| "latest".to_string()));
                return Ok(orca_runtime::command::exec::ExecCommandRequest {
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    output_format: self.output_format.into(),
                    cwd: self.cwd,
                    approval_mode: self.approval_mode,
                    model: self.model,
                    api_key: self.api_key,
                    base_url: self.base_url,
                    verifier: self.verifier,
                    max_budget: self.max_budget,
                    resume: selector,
                    resume_at: resume_args.resume_at,
                    fork: None,
                    continue_latest: false,
                    no_history: false,
                    save_history: false,
                    provider: self.provider,
                    prompt,
                });
            }
            None => (
                self.resume,
                self.continue_latest,
                self.resume_at,
                self.prompt,
            ),
        };
        Ok(orca_runtime::command::exec::ExecCommandRequest {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            output_format: self.output_format.into(),
            cwd: self.cwd,
            approval_mode: self.approval_mode,
            model: self.model,
            api_key: self.api_key,
            base_url: self.base_url,
            verifier: self.verifier,
            max_budget: self.max_budget,
            resume,
            resume_at,
            fork: self.fork,
            continue_latest,
            no_history: self.no_history,
            save_history: self.save_history,
            provider: self.provider,
            prompt,
        })
    }
}

impl From<TrustArgs> for orca_runtime::command::trust::TrustCommandRequest {
    fn from(args: TrustArgs) -> Self {
        use orca_runtime::command::trust::TrustAction as RuntimeTrustAction;

        Self {
            cwd: args.cwd,
            action: match args.action {
                TrustAction::Show => RuntimeTrustAction::Show,
                TrustAction::Add => RuntimeTrustAction::Add,
                TrustAction::Remove => RuntimeTrustAction::Remove,
            },
        }
    }
}

impl From<SubagentWorkerArgs> for orca_runtime::command::launch::SubagentWorkerLaunchRequest {
    fn from(args: SubagentWorkerArgs) -> Self {
        Self {
            app_version: args.app_version,
            cwd: args.cwd,
            child_cwd: args.child_cwd,
            provider: args.provider,
            model: args.model,
            api_key: args.api_key,
            api_key_stdin: args.api_key_stdin,
            base_url: args.base_url,
            session_id: args.session_id,
            agent_id: args.agent_id,
            subagent_depth: args.subagent_depth,
            request_json: args.request_json,
            worktree_repo_root: args.worktree_repo_root,
            worktree_path: args.worktree_path,
        }
    }
}

pub fn run() -> i32 {
    let cli = Cli::parse();

    let retired_history_command = matches!(
        cli.prompt.as_slice(),
        [group, action, ..]
            if group == "history"
                && matches!(
                    action.as_str(),
                    "list" | "show" | "archive" | "delete" | "rename" | "search" | "compress"
                )
    );
    if cli.command.is_none() && retired_history_command {
        eprintln!(
            "error: unrecognized subcommand 'history'\n\nUse `orca --resume` to choose a saved conversation or `orca --resume <SESSION>` to resume one directly."
        );
        return 2;
    }

    if matches!(cli.mode.as_deref(), Some("server")) {
        return orca_runtime::command::launch::run_protocol(protocol_request(
            cli,
            orca_runtime::command::launch::ProtocolMode::Server,
        ));
    }
    if matches!(cli.mode.as_deref(), Some("acp")) {
        return orca_runtime::command::launch::run_protocol(protocol_request(
            cli,
            orca_runtime::command::launch::ProtocolMode::Acp,
        ));
    }

    match cli.command {
        Some(Command::Exec(args)) => match args.into_request() {
            Ok(request) => orca_runtime::command::exec::run(request),
            Err(message) => {
                eprintln!("orca: {message}");
                1
            }
        },
        Some(Command::Workflow(args)) => orca_runtime::workflow::command::run(args.into()),
        Some(Command::Trust(args)) => orca_runtime::command::trust::run(args.into()),
        Some(Command::SubagentWorker(args)) => {
            orca_runtime::command::launch::run_subagent_worker(args.into())
        }
        None => {
            let resume_without_selector = cli.resume.as_deref() == Some("");
            let request = orca_runtime::command::launch::InteractiveLaunchRequest {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                resume: cli.resume.filter(|selector| !selector.is_empty()),
                fork: cli.fork,
                continue_latest: cli.continue_latest,
                session_picker: cli.session_picker || resume_without_selector,
                cwd: cli.cwd,
                model: cli.model,
                mode: cli.mode,
                api_key: cli.api_key,
                base_url: cli.base_url,
                provider: cli.provider,
                prompt: cli.prompt,
            };
            match orca_runtime::command::launch::prepare_interactive(request) {
                Ok(config) => orca_tui::cli::run(config),
                Err(error) => {
                    eprintln!("orca: {error}");
                    1
                }
            }
        }
    }
}

fn protocol_request(
    cli: Cli,
    mode: orca_runtime::command::launch::ProtocolMode,
) -> orca_runtime::command::launch::ProtocolLaunchRequest {
    orca_runtime::command::launch::ProtocolLaunchRequest {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        mode,
        has_command: cli.command.is_some(),
        prompt: cli.prompt,
        cwd: cli.cwd,
        provider: cli.provider,
        model: cli.model,
        api_key: cli.api_key,
        base_url: cli.base_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_exec(args: &[&str]) -> ExecArgs {
        let mut argv = vec!["orca", "exec"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv) {
            Ok(cli) => match cli.command {
                Some(Command::Exec(args)) => args,
                other => panic!("expected exec command, got {other:?}"),
            },
            Err(error) => panic!("parse failed: {error}"),
        }
    }

    #[test]
    fn exec_resume_subcommand_parses_session_id_and_prompt() {
        let args = parse_exec(&[
            "resume",
            "a11864d0",
            "--output-format",
            "jsonl",
            "continue the work",
        ]);
        let request = args.into_request().expect("request");
        assert_eq!(request.resume.as_deref(), Some("a11864d0"));
        assert!(!request.continue_latest);
        assert_eq!(request.prompt, ["continue the work"]);
    }

    #[test]
    fn exec_resume_subcommand_last_parses_without_session_id() {
        let args = parse_exec(&["resume", "--last", "keep going"]);
        let request = args.into_request().expect("request");
        assert_eq!(request.resume.as_deref(), Some("latest"));
        assert_eq!(request.prompt, ["keep going"]);
    }

    #[test]
    fn exec_resume_subcommand_requires_session_id_or_last() {
        let error = Cli::try_parse_from(["orca", "exec", "resume"])
            .expect_err("resume without selector must fail");
        assert!(error.to_string().contains("SESSION_ID"));
    }

    #[test]
    fn exec_prompt_positional_still_parses_without_subcommand() {
        let args = parse_exec(&["inspect", "the", "repo"]);
        let request = args.into_request().expect("request");
        assert!(request.resume.is_none());
        assert_eq!(request.prompt, ["inspect", "the", "repo"]);
    }

    #[test]
    fn exec_resume_flag_still_parses() {
        let args = parse_exec(&["--resume", "latest", "inspect the repo"]);
        let request = args.into_request().expect("request");
        assert_eq!(request.resume.as_deref(), Some("latest"));
        assert_eq!(request.prompt, ["inspect the repo"]);
    }

    #[test]
    fn exec_resume_subcommand_rejects_combined_resume_flag() {
        let args = parse_exec(&["--resume", "old-id", "resume", "new-id", "prompt"]);
        let error = args.into_request().expect_err("combined resume must fail");
        assert!(error.contains("cannot be combined"));
    }

    #[test]
    fn exec_resume_subcommand_rejects_no_history() {
        let args = parse_exec(&["--no-history", "resume", "--last", "prompt"]);
        let error = args
            .into_request()
            .expect_err("no-history resume must fail");
        assert!(error.contains("--no-history"));
    }

    #[test]
    fn exec_resume_subcommand_forwards_budget_scope_options() {
        let args = parse_exec(&["resume", "--last", "--max-budget", "1.5", "prompt"]);
        let request = args.into_request().expect("request");
        assert_eq!(request.max_budget, Some(1.5));
        assert_eq!(request.prompt, ["prompt"]);
    }

    #[test]
    fn exec_resume_subcommand_forwards_message_boundary() {
        let args = parse_exec(&[
            "resume",
            "a11864d0",
            "--resume-at",
            "item_019f1234",
            "continue",
        ]);
        let request = args.into_request().expect("request");
        assert_eq!(request.resume.as_deref(), Some("a11864d0"));
        assert_eq!(request.resume_at.as_deref(), Some("item_019f1234"));
        assert_eq!(request.prompt, ["continue"]);
    }

    #[test]
    fn exec_resume_flag_forwards_message_boundary() {
        let args = parse_exec(&[
            "--resume",
            "latest",
            "--resume-at",
            "item_019f1234",
            "continue",
        ]);
        let request = args.into_request().expect("request");
        assert_eq!(request.resume.as_deref(), Some("latest"));
        assert_eq!(request.resume_at.as_deref(), Some("item_019f1234"));
    }
}
