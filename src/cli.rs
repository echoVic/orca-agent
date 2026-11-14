use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{OutputFormat, ProviderKind};

#[derive(Debug, Parser)]
#[command(name = "orca")]
#[command(version)]
#[command(about = "Orca Agent: a DeepSeek-native terminal coding agent.")]
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
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// Approval mode (new TUI default: full-auto), 'server' for JSON-RPC, or 'acp' for Agent Client Protocol.
    #[arg(long = "mode", alias = "approval-mode")]
    mode: Option<String>,

    /// Enable Full Access (full-auto), including host shell execution, for this session.
    #[arg(long, conflicts_with_all = ["mode", "plan"])]
    yolo: bool,

    /// Use read-only plan mode for this session.
    #[arg(long, conflicts_with_all = ["mode", "yolo"])]
    plan: bool,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Workspace directory.
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, hide = true)]
    provider: Option<ProviderKind>,

    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run in the interactive TUI.
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(hide = true)]
    ComputerUseDriver,
    /// Inspect local configuration, trust, and sandbox readiness without network access.
    Doctor(DoctorArgs),
    /// Inspect disk usage and optionally prune archived sessions.
    Storage(StorageArgs),
    /// Restore files and/or the conversation of a saved session to an earlier turn.
    Rewind(RewindArgs),
    /// Run a task and emit events.
    Exec(ExecArgs),
    /// Serve the JSON-RPC app protocol over standard input and output.
    AppServer(ProtocolArgs),
    /// Serve Agent Client Protocol over standard input and output.
    Acp(ProtocolArgs),
    /// Run and inspect local workflows.
    Workflow(WorkflowArgs),
    /// Inspect or update folder trust.
    Trust(TrustArgs),
    /// Add, list, show, remove, log in to, or log out of MCP servers in the user config.
    Mcp(McpArgs),
    /// Serve shared ACP sessions on a restricted local socket (Unix).
    Daemon(DaemonArgs),
    /// Bridge standard ACP stdio to a running local daemon (Unix).
    AcpBridge(AcpBridgeArgs),
    /// Attach the TUI or a headless prompt to a daemon-owned session (Unix).
    Attach(AttachArgs),
    /// Execute a persisted async subagent task.
    #[command(hide = true)]
    SubagentWorker(SubagentWorkerArgs),
}

#[derive(Debug, Parser)]
struct ProtocolArgs {
    /// Workspace directory.
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,
    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(short = 'm', long)]
    model: Option<String>,
    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,
    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,
    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, hide = true)]
    provider: Option<ProviderKind>,
}

#[derive(Debug, Parser)]
struct DaemonArgs {
    /// Local Unix socket; defaults to ORCA_HOME/acp/daemon.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// The only workspace accepted by this daemon.
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct AcpBridgeArgs {
    /// Local Unix socket; defaults to ORCA_HOME/acp/daemon.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct AttachArgs {
    /// Exact session ID, or 'new' to create a daemon-owned session.
    session: String,
    /// Local Unix socket; defaults to ORCA_HOME/acp/daemon.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Workspace, which must match the daemon.
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,
    /// Run a non-interactive prompt instead of opening the TUI; permission requests are denied.
    #[arg(long = "exec", value_name = "PROMPT")]
    prompt: Option<String>,
}

#[derive(Debug, Parser)]
struct StorageArgs {
    /// Target bytes across session transcripts and image assets.
    #[arg(long)]
    max_bytes: Option<u64>,
    /// Remove archived sessions older than this many days.
    #[arg(long)]
    older_than_days: Option<u64>,
    /// Apply the explicit policy. Without this flag, only preview candidates.
    #[arg(long)]
    apply: bool,
}

#[derive(Debug, Parser)]
struct RewindArgs {
    /// Session id, prefix, or 'latest'. Defaults to the most recent session.
    #[arg(value_name = "SESSION_ID")]
    session: Option<String>,

    /// Use the most recent saved session (explicit alias of the default).
    #[arg(long)]
    last: bool,

    /// Restore the state at this persisted message id (default: the latest user message).
    #[arg(long = "at", value_name = "MESSAGE_ID")]
    at: Option<String>,

    /// What to restore: files, conversation, or both.
    #[arg(long, value_enum, default_value_t = RewindModeArg::Both)]
    mode: RewindModeArg,

    /// Show the restore plan without changing any file or the transcript.
    #[arg(long)]
    dry_run: bool,

    /// Print the stable JSON report.
    #[arg(long)]
    json: bool,

    /// Overwrite files that changed outside tracked tool writes.
    #[arg(long)]
    force: bool,

    /// Roll back an interrupted restore journal by operation id.
    #[arg(long, value_name = "OP_ID")]
    rollback: Option<String>,

    /// Remove checkpoint blobs that no retained snapshot references.
    #[arg(long)]
    prune: bool,
}

impl RewindArgs {
    fn into_request(self) -> orca_runtime::command::rewind::RewindCommandRequest {
        orca_runtime::command::rewind::RewindCommandRequest {
            session: self.session,
            last: self.last,
            at: self.at,
            mode: self.mode.into(),
            dry_run: self.dry_run,
            json: self.json,
            force: self.force,
            rollback: self.rollback,
            prune: self.prune,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RewindModeArg {
    /// Restore files and truncate the conversation.
    Both,
    /// Restore files only.
    Code,
    /// Truncate the conversation only.
    Conversation,
    /// Summarize the conversation from the target onward (not implemented yet).
    SummarizeFrom,
    /// Summarize the conversation up to the target (not implemented yet).
    SummarizeUpTo,
}

impl From<RewindModeArg> for orca_core::rewind::RewindMode {
    fn from(value: RewindModeArg) -> Self {
        match value {
            RewindModeArg::Both => Self::Both,
            RewindModeArg::Code => Self::Code,
            RewindModeArg::Conversation => Self::Conversation,
            RewindModeArg::SummarizeFrom => Self::SummarizeFrom,
            RewindModeArg::SummarizeUpTo => Self::SummarizeUpTo,
        }
    }
}

#[derive(Debug, Parser)]
struct DoctorArgs {
    /// Workspace directory to inspect.
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,

    /// Output format: text (human-readable) or json (stable machine-readable report).
    #[arg(long, value_enum, default_value_t = DoctorFormatArg::Text)]
    format: DoctorFormatArg,

    /// Output the stable JSON report (shorthand for --format json).
    #[arg(long, conflicts_with = "format")]
    json: bool,

    /// Also check the API key with the provider (one GET /models request; not billed).
    #[arg(long)]
    online: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DoctorFormatArg {
    Json,
    Text,
}

#[derive(Debug, Parser)]
#[command(
    override_usage = "orca exec [OPTIONS] [PROMPT]...\n       orca exec [OPTIONS] <COMMAND> [ARGS]"
)]
struct ExecArgs {
    /// Resume a saved conversation as a non-interactive session.
    #[command(subcommand)]
    command: Option<ExecCommand>,

    #[command(flatten)]
    options: ExecOptions,

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

    /// Prompt to execute.
    prompt: Vec<String>,
}

// Keep values from exec and exec resume separate until they can be checked.
// Clap global arguments overwrite their parent values during propagation.
#[derive(Debug, Default, Parser)]
struct ExecOptions {
    /// Output format: text (default, human-readable) or jsonl (machine-readable).
    #[arg(long, value_enum)]
    output_format: Option<OutputFormatArg>,

    /// Output one JSON object per event (shorthand for --output-format jsonl).
    #[arg(long, conflicts_with = "output_format")]
    json: bool,

    /// Workspace directory.
    #[arg(id = "exec_cwd", short = 'C', long = "cwd", visible_alias = "cd")]
    cwd: Option<PathBuf>,

    /// Approval policy for tool actions.
    #[arg(long = "mode", alias = "approval-mode", value_enum)]
    approval_mode: Option<ApprovalMode>,

    /// Enable Full Access (full-auto), including host shell execution, for this run.
    #[arg(id = "exec_yolo", long = "yolo", conflicts_with_all = ["approval_mode", "exec_plan"])]
    yolo: bool,

    /// Use read-only plan mode for this run.
    #[arg(id = "exec_plan", long = "plan", conflicts_with_all = ["approval_mode", "exec_yolo"])]
    plan: bool,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(id = "exec_model", short = 'm', long = "model")]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(id = "exec_api_key", long = "api-key")]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(id = "exec_base_url", long = "base-url")]
    base_url: Option<String>,

    /// Optional verifier command to run after completion.
    #[arg(long)]
    verifier: Option<String>,

    /// Maximum number of model turns for this run (unlimited by default).
    #[arg(long)]
    max_turns: Option<u32>,

    /// Maximum number of tool calls for this run (unlimited by default).
    #[arg(long)]
    max_tool_calls: Option<u32>,

    /// Maximum estimated cost in USD for this run (unlimited by default).
    #[arg(long)]
    max_cost_usd: Option<f64>,

    /// Maximum wall-clock time in seconds for this run (unlimited by default).
    #[arg(long)]
    max_wall_time_secs: Option<u64>,

    /// Maximum estimated USD budget for this run (legacy alias of
    /// --max-cost-usd).
    #[arg(long, hide = true)]
    max_budget: Option<f64>,

    /// Provider implementation (internal, for testing).
    #[arg(id = "exec_provider", long = "provider", value_enum, hide = true)]
    provider: Option<ProviderKind>,
}

#[derive(Debug, Subcommand)]
enum ExecCommand {
    /// Resume a saved conversation by ID, prefix, or 'latest'.
    Resume(ExecResumeArgs),
}

#[derive(Debug, Parser)]
struct ExecResumeArgs {
    #[command(flatten)]
    options: ExecOptions,

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
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TrustAction {
    Show,
    Add,
    Remove,
}

#[derive(Debug, Parser)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Add an MCP server to the user config.
    Add(McpAddArgs),
    /// List MCP servers in the user config.
    List(McpListArgs),
    /// Show one MCP server's configuration.
    Get(McpGetArgs),
    /// Remove an MCP server from the user config.
    Remove(McpRemoveArgs),
    /// Log in to a remote MCP server with OAuth.
    Login(McpLoginArgs),
    /// Log out of a remote MCP server, deleting its saved login.
    Logout(McpLogoutArgs),
}

// `--header`, `--bearer-token-env-var`, `--client-id`, and `--callback-port`
// are documented as requiring `--url` but cannot use clap's `requires`
// attribute for it: clap does not evaluate `requires` for an argument that
// is itself a member of an `ArgGroup` (here, `url`), so it would silently
// never fire. `orca_runtime::command::mcp::build_server` is the real
// enforcement point.
#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("mcp_add_target")
        .required(true)
        .args(["url", "command"]),
))]
struct McpAddArgs {
    /// MCP server name.
    name: String,

    /// Environment variable to set for a stdio server, as KEY=VALUE (repeatable).
    #[arg(short = 'e', long = "env", conflicts_with = "url")]
    env: Vec<String>,

    /// Remote server URL.
    #[arg(long)]
    url: Option<String>,

    /// Remote transport: http or sse (default http).
    #[arg(long)]
    transport: Option<String>,

    /// HTTP header to send to a remote server, as 'Name: value' (repeatable).
    /// Requires --url.
    #[arg(long = "header")]
    header: Vec<String>,

    /// Environment variable holding a bearer token for a remote server.
    /// Requires --url.
    #[arg(long)]
    bearer_token_env_var: Option<String>,

    /// OAuth client id for a remote server. Requires --url.
    #[arg(long)]
    client_id: Option<String>,

    /// Local OAuth redirect callback port for a remote server. Requires
    /// --url.
    #[arg(long)]
    callback_port: Option<u16>,

    /// Stdio server command and its arguments.
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Debug, Parser)]
struct McpListArgs {
    /// Output a JSON array instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct McpGetArgs {
    /// MCP server name.
    name: String,

    /// Output a JSON object instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct McpRemoveArgs {
    /// MCP server name.
    name: String,
}

#[derive(Debug, Parser)]
struct McpLoginArgs {
    /// MCP server name.
    name: String,
}

#[derive(Debug, Parser)]
struct McpLogoutArgs {
    /// MCP server name.
    name: String,
}

impl McpArgs {
    fn into_request(self) -> orca_runtime::command::mcp::McpCommandRequest {
        use orca_runtime::command::mcp::McpCommandRequest;

        match self.command {
            McpCommand::Add(args) => McpCommandRequest::Add(args.into()),
            McpCommand::List(args) => McpCommandRequest::List { json: args.json },
            McpCommand::Get(args) => McpCommandRequest::Get {
                name: args.name,
                json: args.json,
            },
            McpCommand::Remove(args) => McpCommandRequest::Remove { name: args.name },
            McpCommand::Login(args) => McpCommandRequest::Login { name: args.name },
            McpCommand::Logout(args) => McpCommandRequest::Logout { name: args.name },
        }
    }
}

impl From<McpAddArgs> for orca_runtime::command::mcp::McpAddRequest {
    fn from(args: McpAddArgs) -> Self {
        Self {
            name: args.name,
            transport: args.transport,
            env: args.env,
            url: args.url,
            headers: args.header,
            bearer_token_env_var: args.bearer_token_env_var,
            client_id: args.client_id,
            callback_port: args.callback_port,
            command: args.command,
        }
    }
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
    #[arg(short = 'C', long, visible_alias = "cd")]
    cwd: Option<PathBuf>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(short = 'm', long)]
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
    #[arg(short = 'm', long)]
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

impl WorkflowArgs {
    /// Build the workflow request, folding in the global `--cwd` so every workflow
    /// subcommand resolves against the workspace the user asked for.
    fn into_request(
        self,
        global_cwd: Option<PathBuf>,
    ) -> orca_runtime::workflow::command::WorkflowCommandRequest {
        let args = self;
        let global_cwd = global_cwd;
        use orca_runtime::workflow::command::{
            WorkflowCommandRequest, WorkflowListRequest, WorkflowRunRequest, WorkflowWorkerRequest,
        };

        match args.command {
            WorkflowCommand::Run(args) => WorkflowCommandRequest::Run(WorkflowRunRequest {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                cwd: args.cwd.or(global_cwd.clone()),
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
            WorkflowCommand::Source { name } => WorkflowCommandRequest::Source {
                name,
                cwd: global_cwd.clone(),
            },
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
    #[arg(short = 'm', long)]
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

    /// Actor-issued public key used to authenticate detached permission responses.
    #[arg(long, hide = true)]
    permission_response_public_key: String,

    /// Logical child turn identity allocated by the admitting actor.
    #[arg(long, hide = true)]
    child_turn_id: String,

    /// Whether the admitting actor already committed source sequence one.
    #[arg(long, hide = true)]
    activity_start_precommitted: bool,

    /// Preserve the parent's advertisement-only exec mode in a detached worker.
    #[arg(long, hide = true)]
    headless_exec: bool,

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

impl ExecOptions {
    fn merge(&mut self, child: Self) -> Result<(), String> {
        self.output_format = merge_option(
            "--output-format",
            self.output_format.take(),
            child.output_format,
        )?;
        self.cwd = merge_option("--cwd", self.cwd.take(), child.cwd)?;
        self.approval_mode =
            merge_option("--mode", self.approval_mode.take(), child.approval_mode)?;
        self.model = merge_option("--model", self.model.take(), child.model)?;
        self.api_key = merge_option("--api-key", self.api_key.take(), child.api_key)?;
        self.base_url = merge_option("--base-url", self.base_url.take(), child.base_url)?;
        self.verifier = merge_option("--verifier", self.verifier.take(), child.verifier)?;
        self.max_turns = merge_option("--max-turns", self.max_turns.take(), child.max_turns)?;
        self.max_tool_calls = merge_option(
            "--max-tool-calls",
            self.max_tool_calls.take(),
            child.max_tool_calls,
        )?;
        self.max_cost_usd = merge_option(
            "--max-cost-usd",
            self.max_cost_usd.take(),
            child.max_cost_usd,
        )?;
        self.max_wall_time_secs = merge_option(
            "--max-wall-time-secs",
            self.max_wall_time_secs.take(),
            child.max_wall_time_secs,
        )?;
        self.max_budget = merge_option("--max-budget", self.max_budget.take(), child.max_budget)?;
        self.provider = merge_option("--provider", self.provider.take(), child.provider)?;
        self.json |= child.json;
        self.yolo |= child.yolo;
        self.plan |= child.plan;
        self.validate()
    }

    fn validate(&self) -> Result<(), String> {
        if (self.yolo && (self.plan || self.approval_mode.is_some()))
            || (self.plan && self.approval_mode.is_some())
        {
            return Err("--yolo, --plan, and --mode/--approval-mode cannot be combined".into());
        }
        if self.json && self.output_format.is_some() {
            return Err("--json and --output-format cannot be combined".into());
        }
        Ok(())
    }
}

impl ExecArgs {
    fn merge_resume_options(&mut self) -> Result<(), String> {
        if let Some(ExecCommand::Resume(args)) = &mut self.command {
            self.options.merge(std::mem::take(&mut args.options))?;
        }
        self.options.validate()
    }

    fn into_request(mut self) -> Result<orca_runtime::command::exec::ExecCommandRequest, String> {
        self.merge_resume_options()?;
        let approval_mode = if self.options.yolo {
            Some(ApprovalMode::FullAuto)
        } else if self.options.plan {
            Some(ApprovalMode::Plan)
        } else {
            self.options.approval_mode
        };
        let output_format = if self.options.json {
            OutputFormat::Jsonl
        } else {
            self.options
                .output_format
                .unwrap_or(OutputFormatArg::Text)
                .into()
        };
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
                // With --last every positional belongs to the prompt, including
                // the first word Clap assigned to the optional session selector.
                let (selector, prompt) = if resume_args.last {
                    let mut prompt = resume_args.session_id.into_iter().collect::<Vec<_>>();
                    prompt.extend(resume_args.prompt);
                    (Some("latest".to_string()), prompt)
                } else {
                    (resume_args.session_id, resume_args.prompt)
                };
                let resume_at = merge_option("--resume-at", self.resume_at, resume_args.resume_at)?;
                return Ok(orca_runtime::command::exec::ExecCommandRequest {
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    output_format,
                    cwd: self.options.cwd,
                    approval_mode,
                    model: self.options.model,
                    api_key: self.options.api_key,
                    base_url: self.options.base_url,
                    verifier: self.options.verifier,
                    max_budget: self.options.max_budget,
                    max_turns: self.options.max_turns,
                    max_tool_calls: self.options.max_tool_calls,
                    max_cost_usd: self.options.max_cost_usd,
                    max_wall_time_secs: self.options.max_wall_time_secs,
                    resume: selector,
                    resume_at,
                    fork: None,
                    continue_latest: false,
                    no_history: false,
                    save_history: false,
                    provider: self.options.provider.unwrap_or(ProviderKind::DeepSeek),
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
            output_format,
            cwd: self.options.cwd,
            approval_mode,
            model: self.options.model,
            api_key: self.options.api_key,
            base_url: self.options.base_url,
            verifier: self.options.verifier,
            max_budget: self.options.max_budget,
            max_turns: self.options.max_turns,
            max_tool_calls: self.options.max_tool_calls,
            max_cost_usd: self.options.max_cost_usd,
            max_wall_time_secs: self.options.max_wall_time_secs,
            resume,
            resume_at,
            fork: self.fork,
            continue_latest,
            no_history: self.no_history,
            save_history: self.save_history,
            provider: self.options.provider.unwrap_or(ProviderKind::DeepSeek),
            prompt,
        })
    }
}

fn merge_option<T: PartialEq>(
    name: &str,
    root: Option<T>,
    command: Option<T>,
) -> Result<Option<T>, String> {
    match (root, command) {
        (Some(root), Some(command)) if root != command => Err(format!(
            "conflicting {name} values before and after the subcommand"
        )),
        (root, command) => Ok(command.or(root)),
    }
}

impl Cli {
    fn normalize_options(&mut self) -> Result<(), String> {
        if let Some(Command::Exec(args)) = &mut self.command {
            args.merge_resume_options()?;
        }
        let exec_yolo = matches!(&self.command, Some(Command::Exec(args)) if args.options.yolo);
        let exec_plan = matches!(&self.command, Some(Command::Exec(args)) if args.options.plan);
        if (self.yolo || exec_yolo) && (self.mode.is_some() || self.plan || exec_plan) {
            return Err("--yolo cannot be combined with --mode, --approval-mode, or --plan".into());
        }
        if (self.plan || exec_plan) && self.mode.is_some() {
            return Err("--plan cannot be combined with --mode or --approval-mode".into());
        }
        let history_selected = self.resume.is_some()
            || self.fork.is_some()
            || self.continue_latest
            || self.session_picker;
        let legacy_protocol = matches!(self.mode.as_deref(), Some("server" | "acp"));
        let named_protocol = matches!(self.command, Some(Command::AppServer(_) | Command::Acp(_)));
        if legacy_protocol
            && (self.command.is_some() || !self.prompt.is_empty() || history_selected)
        {
            return Err("--mode server/acp cannot be combined with a subcommand, prompt, --resume, --fork, or --continue".into());
        }
        if named_protocol && (history_selected || self.mode.is_some() || self.yolo || self.plan) {
            return Err("protocol subcommands cannot be combined with --resume, --fork, --continue, --mode, --yolo, or --plan".into());
        }
        if self.command.is_some() && history_selected {
            return Err("TUI --resume/--fork/--continue options cannot be combined with a subcommand; use 'exec resume' for headless recovery".into());
        }
        match &mut self.command {
            Some(Command::Exec(args)) => {
                args.options.approval_mode = merge_option(
                    "--mode",
                    self.mode
                        .as_deref()
                        .map(orca_core::config::file::parse_approval_mode_value)
                        .transpose()?,
                    args.options.approval_mode,
                )?;
                if (self.yolo || self.plan) && args.options.approval_mode.is_some() {
                    return Err(
                        "--yolo/--plan cannot be combined with --mode or --approval-mode".into(),
                    );
                }
                args.options.yolo |= self.yolo;
                args.options.plan |= self.plan;
                args.options.cwd = merge_option("--cwd", self.cwd.take(), args.options.cwd.take())?;
                args.options.model =
                    merge_option("--model", self.model.take(), args.options.model.take())?;
                args.options.api_key = merge_option(
                    "--api-key",
                    self.api_key.take(),
                    args.options.api_key.take(),
                )?;
                args.options.base_url = merge_option(
                    "--base-url",
                    self.base_url.take(),
                    args.options.base_url.take(),
                )?;
                args.options.provider = merge_option(
                    "--provider",
                    self.provider.take(),
                    args.options.provider.take(),
                )?;
            }
            Some(Command::AppServer(args) | Command::Acp(args)) => {
                self.cwd = merge_option("--cwd", self.cwd.take(), args.cwd.take())?;
                self.model = merge_option("--model", self.model.take(), args.model.take())?;
                self.api_key = merge_option("--api-key", self.api_key.take(), args.api_key.take())?;
                self.base_url =
                    merge_option("--base-url", self.base_url.take(), args.base_url.take())?;
                self.provider =
                    merge_option("--provider", self.provider.take(), args.provider.take())?;
            }
            Some(Command::Daemon(args)) => {
                args.cwd = merge_option("--cwd", self.cwd.take(), args.cwd.take())?;
                if self.yolo {
                    self.mode = Some("full-auto".into());
                }
                if self.plan {
                    self.mode = Some("plan".into());
                }
            }
            None => {
                if self.yolo {
                    self.mode = Some("full-auto".into());
                }
                if self.plan {
                    self.mode = Some("plan".into());
                }
            }
            _ if self.yolo || self.plan || self.mode.is_some() => {
                return Err("--mode, --yolo, and --plan are supported for interactive sessions, exec, and daemon startup".into());
            }
            _ => {}
        }
        if let Some(Command::Attach(args)) = &mut self.command {
            args.cwd = merge_option("--cwd", self.cwd.take(), args.cwd.take())?;
        }
        if let Some(Command::Trust(args)) = &mut self.command {
            args.cwd = merge_option("--cwd", self.cwd.take(), args.cwd.take())?;
        }
        if let Some(Command::Doctor(args)) = &mut self.command {
            if args.json {
                args.format = DoctorFormatArg::Json;
            }
            args.cwd = merge_option("--cwd", self.cwd.take(), args.cwd.take())?;
        }
        Ok(())
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
            permission_response_public_key: args.permission_response_public_key,
            child_turn_id: args.child_turn_id,
            activity_start_precommitted: args.activity_start_precommitted,
            headless_exec: args.headless_exec,
            worktree_repo_root: args.worktree_repo_root,
            worktree_path: args.worktree_path,
        }
    }
}

pub fn run() -> i32 {
    let mut cli = Cli::parse();
    if let Err(message) = cli.normalize_options() {
        eprintln!("orca: {message}");
        return 2;
    }

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

    if let Some(mode) = match &cli.command {
        Some(Command::AppServer(_)) => Some(orca_runtime::command::launch::ProtocolMode::Server),
        Some(Command::Acp(_)) => Some(orca_runtime::command::launch::ProtocolMode::Acp),
        _ => None,
    } {
        return orca_runtime::command::launch::run_protocol(protocol_request(cli, mode));
    }

    match cli.command {
        Some(Command::AppServer(_) | Command::Acp(_)) => unreachable!("protocol dispatched above"),
        Some(Command::AcpBridge(args)) => local_result(
            resolve_socket(args.socket)
                .and_then(|socket| orca_runtime::acp::daemon::bridge(&socket)),
        ),
        Some(Command::Daemon(args)) => {
            let config = orca_runtime::command::launch::prepare_daemon(
                orca_runtime::command::launch::InteractiveLaunchRequest {
                    app_version: env!("CARGO_PKG_VERSION").into(),
                    resume: None,
                    fork: None,
                    continue_latest: false,
                    session_picker: false,
                    cwd: args.cwd.or(cli.cwd),
                    model: cli.model,
                    mode: cli.mode,
                    api_key: cli.api_key,
                    base_url: cli.base_url,
                    provider: cli.provider.unwrap_or(ProviderKind::DeepSeek),
                    prompt: Vec::new(),
                },
            );
            local_result(config.map_err(std::io::Error::other).and_then(|config| {
                resolve_socket(args.socket)
                    .and_then(|socket| orca_runtime::acp::daemon::run(config, socket))
            }))
        }
        Some(Command::Attach(args)) => {
            let cwd = args
                .cwd
                .or(cli.cwd)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let cwd = match cwd.canonicalize() {
                Ok(cwd) => cwd,
                Err(error) => return local_result(Err(error)),
            };
            let socket = match resolve_socket(args.socket) {
                Ok(socket) => socket,
                Err(error) => return local_result(Err(error)),
            };
            if let Some(prompt) = args.prompt {
                return local_result(orca_runtime::acp::client::run_headless(
                    &socket,
                    &cwd,
                    &args.session,
                    prompt,
                ));
            }
            let config = orca_runtime::command::launch::prepare_interactive(
                orca_runtime::command::launch::InteractiveLaunchRequest {
                    app_version: env!("CARGO_PKG_VERSION").into(),
                    resume: None,
                    fork: None,
                    continue_latest: false,
                    session_picker: false,
                    cwd: Some(cwd),
                    model: cli.model,
                    mode: None,
                    api_key: None,
                    base_url: None,
                    provider: cli.provider.unwrap_or(ProviderKind::DeepSeek),
                    prompt: Vec::new(),
                },
            );
            match config {
                Ok(config) => orca_tui::cli::run_attached(config, socket, args.session),
                Err(error) => local_result(Err(std::io::Error::other(error))),
            }
        }
        Some(Command::Storage(args)) => {
            let policy = orca_runtime::thread_store::SessionRetentionPolicy {
                max_bytes: args.max_bytes,
                older_than_days: args.older_than_days,
            };
            match orca_runtime::thread_store::retain_sessions(&policy, args.apply) {
                Ok(report) => {
                    println!(
                        "{}: {} bytes; {} bytes after cleanup; quota satisfied: {}",
                        if report.applied { "Applied" } else { "Dry run" },
                        report.bytes_before,
                        report.bytes_after,
                        report.quota_satisfied,
                    );
                    for candidate in &report.candidates {
                        let status = if report.deleted.contains(&candidate.path) {
                            "deleted"
                        } else if report.skipped.contains(&candidate.path) {
                            "skipped"
                        } else {
                            "candidate"
                        };
                        println!(
                            "{status}: {} bytes {}",
                            candidate.bytes,
                            candidate.path.display()
                        );
                    }
                    0
                }
                Err(error) => {
                    eprintln!("orca: {error}");
                    1
                }
            }
        }
        Some(Command::Rewind(args)) => orca_runtime::command::rewind::run(args.into_request()),
        Some(Command::Doctor(args)) => {
            orca_runtime::diagnostics::run(orca_runtime::diagnostics::DoctorRequest {
                cwd: args.cwd,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                online: args.online,
                format: match args.format {
                    DoctorFormatArg::Json => orca_runtime::diagnostics::DoctorOutputFormat::Json,
                    DoctorFormatArg::Text => orca_runtime::diagnostics::DoctorOutputFormat::Text,
                },
            })
        }
        Some(Command::Exec(args)) => match args.into_request() {
            Ok(request) => orca_runtime::command::exec::run(request),
            Err(message) => {
                eprintln!("orca: {message}");
                1
            }
        },
        Some(Command::Workflow(args)) => {
            orca_runtime::workflow::command::run(args.into_request(cli.cwd))
        }
        Some(Command::Trust(args)) => orca_runtime::command::trust::run(args.into()),
        Some(Command::Mcp(args)) => orca_runtime::command::mcp::run(args.into_request()),
        Some(Command::ComputerUseDriver) => orca_runtime::command::computer_use_driver::run(),
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
                provider: cli.provider.unwrap_or(ProviderKind::DeepSeek),
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

fn resolve_socket(socket: Option<PathBuf>) -> std::io::Result<PathBuf> {
    socket
        .map(Ok)
        .unwrap_or_else(orca_runtime::acp::daemon::default_socket_path)
}

fn local_result(result: std::io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: {error}");
            1
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
        has_command: cli.command.is_some()
            && !matches!(cli.command, Some(Command::AppServer(_) | Command::Acp(_))),
        prompt: cli.prompt,
        cwd: cli.cwd,
        provider: cli.provider.unwrap_or(ProviderKind::DeepSeek),
        model: cli.model,
        api_key: cli.api_key,
        base_url: cli.base_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::Value;

    #[test]
    fn subagent_worker_accepts_headless_advertisement_signal() {
        let parsed = Cli::try_parse_from([
            "orca",
            "subagent-worker",
            "--cwd",
            "/workspace",
            "--child-cwd",
            "/workspace",
            "--session-id",
            "session",
            "--agent-id",
            "agent",
            "--subagent-depth",
            "1",
            "--request-json",
            "{}",
            "--permission-response-public-key",
            "key",
            "--child-turn-id",
            "turn",
            "--headless-exec",
        ]);
        let parsed = parsed.expect("headless worker flag");
        let Some(Command::SubagentWorker(args)) = parsed.command else {
            panic!("worker command");
        };
        let request: orca_runtime::command::launch::SubagentWorkerLaunchRequest = args.into();
        assert!(request.headless_exec);
    }

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
    fn yolo_selects_full_auto_for_exec_and_resume() {
        for args in [
            vec!["--yolo", "fix the test"],
            vec!["resume", "--last", "--yolo", "continue"],
            vec!["--yolo", "--resume", "session-id", "continue"],
        ] {
            let request = parse_exec(&args).into_request().expect("exec request");
            assert_eq!(request.approval_mode, Some(ApprovalMode::FullAuto));
        }
    }

    #[test]
    fn yolo_selects_full_auto_for_interactive_launch_and_resume() {
        for args in [
            vec!["orca", "--yolo"],
            vec!["orca", "--yolo", "--continue"],
            vec!["orca", "--resume", "session-id", "--yolo"],
        ] {
            let mut cli = Cli::try_parse_from(args).expect("interactive Full Access flag");
            cli.normalize_options().expect("normalize Full Access flag");
            assert_eq!(cli.mode.as_deref(), Some("full-auto"));
        }
    }

    #[test]
    fn yolo_before_exec_is_forwarded_and_rejects_cross_level_mode_conflicts() {
        let mut cli = Cli::try_parse_from(["orca", "--yolo", "exec", "fix the test"])
            .expect("root yolo with exec");
        cli.normalize_options().expect("forward Full Access flag");
        let Some(Command::Exec(args)) = cli.command else {
            panic!("exec command");
        };
        assert_eq!(
            args.into_request().unwrap().approval_mode,
            Some(ApprovalMode::FullAuto)
        );
        for args in [
            vec!["orca", "--yolo", "exec", "--mode", "suggest", "fix"],
            vec!["orca", "--mode", "suggest", "exec", "--yolo", "fix"],
            vec!["orca", "--mode", "server", "exec", "--yolo", "fix"],
        ] {
            let mut cli = Cli::try_parse_from(args).expect("separate flag scopes");
            assert!(
                cli.normalize_options()
                    .unwrap_err()
                    .contains("cannot be combined")
            );
        }
    }

    #[test]
    fn yolo_is_not_silently_ignored_on_other_commands() {
        for args in [
            vec!["orca", "--yolo", "doctor"],
            vec!["orca", "--yolo", "attach", "new"],
            vec!["orca", "--yolo", "workflow", "list"],
            vec!["orca", "--yolo", "storage"],
        ] {
            let mut cli = Cli::try_parse_from(args).expect("root flag");
            assert!(cli.normalize_options().is_err());
        }
        let mut cli = Cli::try_parse_from(["orca", "--yolo", "daemon"]).unwrap();
        cli.normalize_options().unwrap();
        assert_eq!(cli.mode.as_deref(), Some("full-auto"));
    }

    #[test]
    fn yolo_rejects_a_second_explicit_mode() {
        for args in [
            vec!["orca", "--yolo", "--mode", "suggest"],
            vec!["orca", "exec", "--yolo", "--mode", "plan", "inspect"],
            vec!["orca", "--yolo", "--mode", "acp"],
        ] {
            let error = Cli::try_parse_from(args).expect_err("conflicting mode");
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn cli_habits_root_exec_options_are_forwarded() {
        for argv in [
            vec![
                "orca",
                "--mode",
                "plan",
                "--model",
                "custom-model",
                "--cwd",
                "/workspace",
                "--api-key",
                "cli-test-key",
                "--base-url",
                "http://localhost:9000",
                "--provider",
                "mock",
                "exec",
                "inspect",
            ],
            vec![
                "orca",
                "exec",
                "--mode",
                "plan",
                "--model",
                "custom-model",
                "--cwd",
                "/workspace",
                "--api-key",
                "cli-test-key",
                "--base-url",
                "http://localhost:9000",
                "--provider",
                "mock",
                "inspect",
            ],
            vec![
                "orca",
                "--mode",
                "plan",
                "--model",
                "custom-model",
                "exec",
                "resume",
                "latest",
                "--cwd",
                "/workspace",
                "--api-key",
                "cli-test-key",
                "--base-url",
                "http://localhost:9000",
                "--provider",
                "mock",
                "inspect",
            ],
        ] {
            let mut cli = Cli::try_parse_from(argv).unwrap();
            cli.normalize_options().unwrap();
            let Some(Command::Exec(args)) = cli.command else {
                panic!("exec");
            };
            let request = args.into_request().unwrap();
            assert_eq!(request.approval_mode, Some(ApprovalMode::Plan));
            assert_eq!(request.model.as_deref(), Some("custom-model"));
            assert_eq!(request.cwd, Some(PathBuf::from("/workspace")));
            assert_eq!(request.api_key.as_deref(), Some("cli-test-key"));
            assert_eq!(request.base_url.as_deref(), Some("http://localhost:9000"));
            assert_eq!(request.provider, ProviderKind::Mock);
        }
    }

    #[test]
    fn cli_habits_conflicting_root_exec_values_are_rejected() {
        for argv in [
            vec![
                "orca", "--mode", "suggest", "exec", "--mode", "plan", "inspect",
            ],
            vec![
                "orca", "--model", "one", "exec", "--model", "two", "inspect",
            ],
            vec!["orca", "--cwd", "/one", "exec", "--cwd", "/two", "inspect"],
            vec![
                "orca",
                "--api-key",
                "first-test-key",
                "exec",
                "--api-key",
                "second-test-key",
                "inspect",
            ],
            vec![
                "orca",
                "--base-url",
                "http://localhost:9000",
                "exec",
                "--base-url",
                "http://localhost:9001",
                "inspect",
            ],
            vec![
                "orca",
                "--provider",
                "deepseek",
                "exec",
                "--provider",
                "mock",
                "inspect",
            ],
        ] {
            let mut cli = Cli::try_parse_from(argv).unwrap();
            let error = cli.normalize_options().expect_err("conflicting root value");
            assert!(error.contains("conflict"), "{error}");
            assert!(!error.contains("first-test-key"));
            assert!(!error.contains("second-test-key"));
        }
    }

    #[test]
    fn cli_habits_equal_root_exec_values_are_accepted() {
        let mut cli = Cli::try_parse_from([
            "orca", "--mode", "plan", "--model", "same", "exec", "--mode", "plan", "--model",
            "same", "inspect",
        ])
        .unwrap();
        cli.normalize_options().unwrap();
        let Some(Command::Exec(args)) = cli.command else {
            panic!("exec");
        };
        let request = args.into_request().unwrap();
        assert_eq!(request.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(request.model.as_deref(), Some("same"));
    }

    #[test]
    fn cli_habits_exec_last_preserves_every_prompt_word() {
        let request = parse_exec(&["resume", "--last", "fix", "failing", "test"])
            .into_request()
            .unwrap();
        assert_eq!(request.resume.as_deref(), Some("latest"));
        assert_eq!(request.prompt, ["fix", "failing", "test"]);
        let request = parse_exec(&["resume", "--last"]).into_request().unwrap();
        assert!(request.prompt.is_empty());
    }

    #[test]
    fn cli_habits_exec_resume_boundary_works_before_the_subcommand() {
        let request = parse_exec(&["--resume-at", "message", "resume", "session", "continue"])
            .into_request()
            .unwrap();
        assert_eq!(request.resume.as_deref(), Some("session"));
        assert_eq!(request.resume_at.as_deref(), Some("message"));
    }

    #[test]
    fn cli_habits_exec_resume_conflicting_boundaries_are_rejected() {
        let args = parse_exec(&[
            "--resume-at",
            "one",
            "resume",
            "session",
            "--resume-at",
            "two",
            "continue",
        ]);
        assert!(args.into_request().unwrap_err().contains("conflict"));
    }

    #[test]
    fn cli_habits_protocol_modes_reject_history_selectors() {
        for protocol in ["server", "acp"] {
            for selector in [
                vec!["--resume", "session"],
                vec!["--fork", "session"],
                vec!["--continue"],
                vec!["--resume"],
            ] {
                let mut argv = vec!["orca", "--mode", protocol];
                argv.extend(selector);
                let mut cli = Cli::try_parse_from(argv).unwrap();
                assert!(
                    cli.normalize_options()
                        .unwrap_err()
                        .contains("cannot be combined")
                );
            }
        }
    }

    #[test]
    fn cli_habits_unused_root_modes_are_rejected() {
        for argv in [
            vec!["orca", "--mode", "plan", "doctor"],
            vec!["orca", "--mode", "suggest", "mcp", "list"],
            vec!["orca", "--continue", "daemon"],
        ] {
            let mut cli = Cli::try_parse_from(argv).unwrap();
            assert!(cli.normalize_options().is_err());
        }
    }

    #[test]
    fn cli_habits_plan_shortcut_works_before_and_after_exec() {
        for argv in [
            vec!["orca", "--plan", "exec", "inspect"],
            vec!["orca", "exec", "--plan", "inspect"],
            vec!["orca", "exec", "resume", "--last", "--plan", "inspect"],
        ] {
            let mut cli = Cli::try_parse_from(argv).unwrap();
            cli.normalize_options().unwrap();
            let Some(Command::Exec(args)) = cli.command else {
                panic!("exec");
            };
            assert_eq!(
                args.into_request().unwrap().approval_mode,
                Some(ApprovalMode::Plan)
            );
        }
        let mut cli = Cli::try_parse_from(["orca", "--plan"]).unwrap();
        cli.normalize_options().unwrap();
        assert_eq!(cli.mode.as_deref(), Some("plan"));
    }

    #[test]
    fn cli_habits_named_protocol_commands_use_protocol_requests() {
        for (name, mode) in [
            (
                "app-server",
                orca_runtime::command::launch::ProtocolMode::Server,
            ),
            ("acp", orca_runtime::command::launch::ProtocolMode::Acp),
        ] {
            let mut cli = Cli::try_parse_from([
                "orca",
                name,
                "--model",
                "custom-model",
                "--cwd",
                "/workspace",
                "--provider",
                "mock",
            ])
            .unwrap();
            assert!(cli.command.is_some(), "named protocol must be a subcommand");
            cli.normalize_options().unwrap();
            let request = protocol_request(cli, mode);
            assert!(!request.has_command);
            assert!(request.prompt.is_empty());
            assert_eq!(request.model.as_deref(), Some("custom-model"));
            assert_eq!(request.cwd, Some(PathBuf::from("/workspace")));
            assert_eq!(request.provider, ProviderKind::Mock);
        }
    }

    #[test]
    fn cli_habits_json_shorthand_selects_the_documented_format() {
        for argv in [
            vec!["--json", "hello"],
            vec!["resume", "--last", "--json", "hello"],
        ] {
            let request = parse_exec(&argv).into_request().unwrap();
            assert_eq!(request.output_format, OutputFormat::Jsonl);
        }
        let mut cli = Cli::try_parse_from(["orca", "doctor", "--json"]).unwrap();
        cli.normalize_options().unwrap();
        let Some(Command::Doctor(args)) = cli.command else {
            panic!("doctor");
        };
        assert_eq!(args.format, DoctorFormatArg::Json);
        assert!(!args.online, "doctor stays offline by default");
    }

    #[test]
    fn doctor_online_parses() {
        let cli = Cli::try_parse_from(["orca", "doctor", "--online", "--json"]).unwrap();
        let Some(Command::Doctor(args)) = cli.command else {
            panic!("doctor");
        };
        assert!(args.online);
    }

    #[test]
    fn cli_habits_familiar_model_and_directory_aliases_are_accepted() {
        for argv in [
            vec!["orca", "-m", "model", "-C", "/workspace"],
            vec!["orca", "--cd", "/workspace", "exec", "-m", "model", "hello"],
            vec![
                "orca",
                "exec",
                "resume",
                "--last",
                "-m",
                "model",
                "--cd",
                "/workspace",
                "hello",
            ],
        ] {
            let mut cli = Cli::try_parse_from(argv).unwrap();
            cli.normalize_options().unwrap();
            if let Some(Command::Exec(args)) = cli.command {
                let request = args.into_request().unwrap();
                assert_eq!(request.model.as_deref(), Some("model"));
                assert_eq!(request.cwd, Some(PathBuf::from("/workspace")));
            } else {
                assert_eq!(cli.model.as_deref(), Some("model"));
                assert_eq!(cli.cwd, Some(PathBuf::from("/workspace")));
            }
        }
    }

    #[test]
    fn cli_habits_equal_options_across_all_exec_scopes_are_accepted() {
        let mut cli = Cli::try_parse_from([
            "orca",
            "--mode",
            "plan",
            "-m",
            "same",
            "exec",
            "--mode",
            "plan",
            "-m",
            "same",
            "--output-format",
            "jsonl",
            "--resume-at",
            "message",
            "resume",
            "--last",
            "--mode",
            "plan",
            "-m",
            "same",
            "--output-format",
            "jsonl",
            "--resume-at",
            "message",
            "inspect",
        ])
        .unwrap();
        cli.normalize_options().unwrap();
        let Some(Command::Exec(args)) = cli.command else {
            panic!("exec");
        };
        let request = args.into_request().unwrap();
        assert_eq!(request.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(request.model.as_deref(), Some("same"));
        assert_eq!(request.output_format, OutputFormat::Jsonl);
        assert_eq!(request.resume_at.as_deref(), Some("message"));
    }

    #[test]
    fn cli_habits_exec_resume_scope_conflicts_are_rejected() {
        for (flag, first, second) in [
            ("--model", "one", "two"),
            ("--cwd", "/one", "/two"),
            ("--api-key", "first-test-key", "second-test-key"),
            ("--base-url", "http://localhost:1", "http://localhost:2"),
            ("--provider", "deepseek", "mock"),
            ("--mode", "plan", "full-auto"),
            ("--output-format", "text", "jsonl"),
            ("--max-turns", "1", "2"),
        ] {
            let args = parse_exec(&[flag, first, "resume", "--last", flag, second, "inspect"]);
            let error = args.into_request().unwrap_err();
            assert!(error.contains("conflict"), "{error}");
            assert!(!error.contains("first-test-key") && !error.contains("second-test-key"));
        }
        for argv in [
            vec![
                "--plan",
                "resume",
                "--last",
                "--mode",
                "full-auto",
                "inspect",
            ],
            vec!["--yolo", "resume", "--last", "--plan", "inspect"],
            vec![
                "--output-format",
                "text",
                "resume",
                "--last",
                "--json",
                "inspect",
            ],
            vec![
                "--json",
                "resume",
                "--last",
                "--output-format",
                "jsonl",
                "inspect",
            ],
        ] {
            assert!(parse_exec(&argv).into_request().is_err(), "{argv:?}");
        }
    }

    #[test]
    fn public_cli_manifest_matches_visible_clap_surface() {
        let manifest: Value =
            serde_json::from_str(include_str!("../docs/public-cli-manifest.json"))
                .expect("valid public CLI manifest");
        assert_eq!(manifest["schemaVersion"], 1);
        assert_eq!(manifest["package"], "@blade-ai/orca");
        assert_eq!(manifest["website"], "https://orcaagent.dev");

        let command = Cli::command();
        let visible = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_string())
            .collect::<Vec<_>>();
        let expected = manifest["commands"]["root"]
            .as_array()
            .expect("root commands")
            .iter()
            .map(|value| value.as_str().expect("command name").to_string())
            .collect::<Vec<_>>();
        assert_eq!(visible, expected);

        let doctor = command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "doctor")
            .expect("doctor command");

        for subcommand in command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
        {
            let name = subcommand.get_name();
            let expected = manifest["commands"][name]
                .as_object()
                .unwrap_or_else(|| panic!("manifest entry missing for {name}"));
            let options = subcommand
                .get_arguments()
                .filter(|argument| !argument.is_hide_set())
                .filter_map(|argument| argument.get_long().map(str::to_string))
                .collect::<Vec<_>>();
            let expected_options = expected["options"]
                .as_array()
                .unwrap_or_else(|| panic!("options missing for {name}"))
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("option name")
                        .trim_start_matches("--")
                        .to_string()
                })
                .collect::<Vec<_>>();
            assert_eq!(options, expected_options, "options drifted for {name}");

            let subcommands = subcommand
                .get_subcommands()
                .filter(|child| !child.is_hide_set())
                .map(|child| child.get_name().to_string())
                .filter(|child| child != "help")
                .collect::<Vec<_>>();
            let expected_subcommands = expected["subcommands"]
                .as_array()
                .unwrap_or_else(|| panic!("subcommands missing for {name}"))
                .iter()
                .map(|value| value.as_str().expect("subcommand name").to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                subcommands, expected_subcommands,
                "subcommands drifted for {name}"
            );
        }

        let doctor_options = doctor
            .get_arguments()
            .filter(|argument| !argument.is_hide_set())
            .filter_map(|argument| argument.get_long())
            .collect::<Vec<_>>();
        assert_eq!(doctor_options, ["cwd", "format", "json", "online"]);
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
    fn exec_forwards_explicit_budget_dimensions() {
        let args = parse_exec(&[
            "--max-turns",
            "8",
            "--max-tool-calls",
            "64",
            "--max-cost-usd",
            "1.25",
            "--max-wall-time-secs",
            "3600",
            "prompt",
        ]);
        let request = args.into_request().expect("request");
        assert_eq!(request.max_turns, Some(8));
        assert_eq!(request.max_tool_calls, Some(64));
        assert_eq!(request.max_cost_usd, Some(1.25));
        assert_eq!(request.max_wall_time_secs, Some(3600));
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

    fn parse_mcp(args: &[&str]) -> McpArgs {
        let mut argv = vec!["orca", "mcp"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv) {
            Ok(cli) => match cli.command {
                Some(Command::Mcp(args)) => args,
                other => panic!("expected mcp command, got {other:?}"),
            },
            Err(error) => panic!("parse failed: {error}"),
        }
    }

    #[test]
    fn mcp_add_parses_a_stdio_command_after_double_dash() {
        use orca_runtime::command::mcp::McpCommandRequest;

        let args = parse_mcp(&[
            "add",
            "docs",
            "-e",
            "API_KEY=secret",
            "--",
            "npx",
            "-y",
            "docs-mcp",
        ]);
        let request = args.into_request();
        match request {
            McpCommandRequest::Add(add) => {
                assert_eq!(add.name, "docs");
                assert_eq!(add.env, ["API_KEY=secret"]);
                assert_eq!(add.command, ["npx", "-y", "docs-mcp"]);
                assert!(add.url.is_none());
            }
            other => panic!("expected an add request, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_parses_remote_options() {
        use orca_runtime::command::mcp::McpCommandRequest;

        let args = parse_mcp(&[
            "add",
            "search",
            "--url",
            "https://example.test/mcp",
            "--transport",
            "sse",
            "--header",
            "Authorization: Bearer t",
            "--bearer-token-env-var",
            "TOK",
            "--client-id",
            "CID",
            "--callback-port",
            "8123",
        ]);
        let request = args.into_request();
        match request {
            McpCommandRequest::Add(add) => {
                assert_eq!(add.name, "search");
                assert_eq!(add.url.as_deref(), Some("https://example.test/mcp"));
                assert_eq!(add.transport.as_deref(), Some("sse"));
                assert_eq!(add.headers, ["Authorization: Bearer t"]);
                assert_eq!(add.bearer_token_env_var.as_deref(), Some("TOK"));
                assert_eq!(add.client_id.as_deref(), Some("CID"));
                assert_eq!(add.callback_port, Some(8123));
                assert!(add.command.is_empty());
            }
            other => panic!("expected an add request, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_requires_exactly_one_of_url_or_command() {
        let neither = Cli::try_parse_from(["orca", "mcp", "add", "docs"]);
        assert!(neither.is_err(), "neither url nor command must be rejected");

        let both = Cli::try_parse_from([
            "orca",
            "mcp",
            "add",
            "docs",
            "--url",
            "https://example.test",
            "--",
            "true",
        ]);
        assert!(both.is_err(), "both url and command must be rejected");
    }

    #[test]
    fn mcp_env_conflicts_with_url() {
        let result = Cli::try_parse_from([
            "orca",
            "mcp",
            "add",
            "docs",
            "--url",
            "https://example.test",
            "-e",
            "FOO=bar",
        ]);
        assert!(result.is_err(), "-e/--env must conflict with --url");
    }

    #[test]
    fn mcp_list_and_get_and_remove_parse_into_requests() {
        use orca_runtime::command::mcp::McpCommandRequest;

        let list = parse_mcp(&["list", "--json"]).into_request();
        assert!(matches!(list, McpCommandRequest::List { json: true }));

        let get = parse_mcp(&["get", "docs"]).into_request();
        match get {
            McpCommandRequest::Get { name, json } => {
                assert_eq!(name, "docs");
                assert!(!json);
            }
            other => panic!("expected a get request, got {other:?}"),
        }

        let remove = parse_mcp(&["remove", "docs"]).into_request();
        match remove {
            McpCommandRequest::Remove { name } => assert_eq!(name, "docs"),
            other => panic!("expected a remove request, got {other:?}"),
        }
    }

    #[test]
    fn mcp_login_and_logout_parse_into_requests() {
        use orca_runtime::command::mcp::McpCommandRequest;

        let login = parse_mcp(&["login", "docs"]).into_request();
        match login {
            McpCommandRequest::Login { name } => assert_eq!(name, "docs"),
            other => panic!("expected a login request, got {other:?}"),
        }

        let logout = parse_mcp(&["logout", "docs"]).into_request();
        match logout {
            McpCommandRequest::Logout { name } => assert_eq!(name, "docs"),
            other => panic!("expected a logout request, got {other:?}"),
        }
    }

    #[test]
    fn bare_mcp_requires_a_subcommand() {
        let error = Cli::try_parse_from(["orca", "mcp"]).expect_err("bare mcp must fail");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "unexpected error kind: {error}"
        );
        assert!(
            error.to_string().contains("Usage: orca mcp <COMMAND>"),
            "expected usage in output: {error}"
        );
    }
}
