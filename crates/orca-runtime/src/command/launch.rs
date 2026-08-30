use std::io::{self, Read};
use std::path::PathBuf;

use base64::Engine;

use orca_core::config::file::{ConfigOverrides, parse_approval_mode_value};
use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};

use crate::subagent::SubagentRequest;
use crate::subagent_async_worker::{self, AsyncSubagentWorkerInput, AsyncSubagentWorktree};

use super::config::{DesktopNotifications, RunConfigRequest, build_run_config};
use super::exec::resolve_history_mode;

const MAX_WORKER_API_KEY_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolMode {
    Server,
    Acp,
}

#[derive(Clone, Debug)]
pub struct ProtocolLaunchRequest {
    pub app_version: String,
    pub mode: ProtocolMode,
    pub has_command: bool,
    pub prompt: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InteractiveLaunchRequest {
    pub app_version: String,
    pub resume: Option<String>,
    pub fork: Option<String>,
    pub continue_latest: bool,
    pub session_picker: bool,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub provider: ProviderKind,
    pub prompt: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SubagentWorkerLaunchRequest {
    pub app_version: String,
    pub cwd: PathBuf,
    pub child_cwd: PathBuf,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_stdin: bool,
    pub base_url: Option<String>,
    pub session_id: String,
    pub agent_id: String,
    pub subagent_depth: u32,
    pub request_json: String,
    pub worktree_repo_root: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
    pub permission_response_public_key: String,
}

pub fn prepare_interactive(request: InteractiveLaunchRequest) -> Result<RunConfig, String> {
    let resume_like = request.resume.is_some() as u8
        + request.fork.is_some() as u8
        + request.continue_latest as u8;
    if resume_like > 1 {
        return Err("--resume, --fork, and --continue are mutually exclusive".to_string());
    }
    let cwd = request
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mode = request
        .mode
        .as_deref()
        .map(parse_approval_mode_value)
        .transpose()?;
    let history_mode = resolve_history_mode(
        request.resume,
        request.fork,
        request.continue_latest,
        None,
        HistoryMode::Record,
    );
    let mut config_request = RunConfigRequest::new(request.app_version, cwd.clone());
    config_request.runtime_cwd = Some(cwd);
    config_request.prompt = request.prompt.join(" ");
    config_request.output_format = OutputFormat::Text;
    config_request.provider = request.provider;
    config_request.history_mode = history_mode;
    config_request.show_session_picker = request.session_picker;
    config_request.overrides = ConfigOverrides {
        model: request.model,
        mode,
        api_key: request.api_key,
        base_url: request.base_url,
        reasoning_effort: None,
    };
    build_run_config(config_request)
}

pub fn run_protocol(request: ProtocolLaunchRequest) -> i32 {
    if let Err(error) = validate_protocol_request(&request) {
        eprintln!("orca: {error}");
        return 1;
    }
    let cwd = request
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut config_request = RunConfigRequest::new(request.app_version, cwd.clone());
    config_request.runtime_cwd = Some(cwd);
    config_request.output_format = OutputFormat::Jsonl;
    config_request.provider = request.provider;
    config_request.history_mode = HistoryMode::Record;
    config_request.desktop_notifications = DesktopNotifications::Disabled;
    config_request.overrides = ConfigOverrides {
        model: request.model,
        mode: None,
        api_key: request.api_key,
        base_url: request.base_url,
        reasoning_effort: None,
    };
    let config = match build_run_config(config_request) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    match request.mode {
        ProtocolMode::Server => {
            crate::server::run(crate::server::ServerConfig { run_config: config })
        }
        ProtocolMode::Acp => run_acp(config),
    }
}

fn run_acp(config: RunConfig) -> i32 {
    let host = match crate::runtime_host::RuntimeHost::start() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("orca: failed to start runtime host: {error}");
            return 1;
        }
    };
    let exit_code = crate::acp::run_with_surface_host(host.surface_handle(), config);
    drop(host);
    exit_code
}

pub fn run_subagent_worker(request: SubagentWorkerLaunchRequest) -> i32 {
    let stdin = io::stdin();
    run_subagent_worker_with_reader(request, stdin)
}

fn run_subagent_worker_with_reader(request: SubagentWorkerLaunchRequest, reader: impl Read) -> i32 {
    let subagent_request: SubagentRequest = match serde_json::from_str(&request.request_json) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("orca: invalid subagent worker request JSON: {error}");
            return 1;
        }
    };
    let api_key =
        match resolve_worker_api_key_from_reader(request.api_key, request.api_key_stdin, reader) {
            Ok(api_key) => api_key,
            Err(error) => {
                eprintln!("orca: {error}");
                return 1;
            }
        };
    let public_key_bytes = match base64::engine::general_purpose::STANDARD
        .decode(request.permission_response_public_key.as_bytes())
    {
        Ok(bytes) if bytes.len() == 32 => {
            let mut key = [0_u8; 32];
            key.copy_from_slice(&bytes);
            key
        }
        _ => {
            eprintln!("orca: invalid detached permission response public key");
            return 1;
        }
    };
    let worktree = match validate_worktree_pair(request.worktree_repo_root, request.worktree_path) {
        Ok(worktree) => worktree,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let mut config_request = RunConfigRequest::new(request.app_version, request.cwd.clone());
    config_request.runtime_cwd = Some(request.cwd.clone());
    config_request.output_format = OutputFormat::Jsonl;
    config_request.provider = request.provider;
    config_request.history_mode = HistoryMode::Disabled;
    config_request.desktop_notifications = DesktopNotifications::Disabled;
    config_request.overrides = ConfigOverrides {
        model: request.model,
        mode: None,
        api_key,
        base_url: request.base_url,
        reasoning_effort: None,
    };
    let mut config = match build_run_config(config_request) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    if let Some(snapshot) = subagent_request.delegation.as_ref() {
        snapshot.apply_to(&mut config, subagent_request.model.clone());
    }

    subagent_async_worker::run_async_subagent_worker(AsyncSubagentWorkerInput {
        config,
        cwd: request.cwd,
        child_cwd: request.child_cwd,
        task_session_id: request.session_id,
        agent_id: request.agent_id,
        request: subagent_request,
        child_depth: request.subagent_depth,
        worktree,
        permission_response_public_key: public_key_bytes,
    })
}

fn validate_protocol_request(request: &ProtocolLaunchRequest) -> Result<(), String> {
    if request.has_command || !request.prompt.is_empty() {
        let mode = match request.mode {
            ProtocolMode::Server => "server",
            ProtocolMode::Acp => "acp",
        };
        return Err(format!(
            "--mode={mode} cannot be combined with a subcommand or prompt"
        ));
    }
    Ok(())
}

fn validate_worktree_pair(
    repo_root: Option<PathBuf>,
    path: Option<PathBuf>,
) -> Result<Option<AsyncSubagentWorktree>, String> {
    match (repo_root, path) {
        (Some(repo_root), Some(path)) => Ok(Some(AsyncSubagentWorktree { repo_root, path })),
        (None, None) => Ok(None),
        _ => Err("--worktree-repo-root and --worktree-path must be provided together".to_string()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_launch_rejects_subcommands_and_prompts() {
        let request = ProtocolLaunchRequest {
            app_version: "0.2.55".to_string(),
            mode: ProtocolMode::Server,
            has_command: true,
            prompt: Vec::new(),
            cwd: None,
            provider: ProviderKind::Mock,
            model: None,
            api_key: None,
            base_url: None,
        };
        assert_eq!(
            validate_protocol_request(&request).unwrap_err(),
            "--mode=server cannot be combined with a subcommand or prompt"
        );
    }

    #[test]
    fn interactive_launch_rejects_multiple_resume_selectors() {
        let request = InteractiveLaunchRequest {
            app_version: "0.2.55".to_string(),
            resume: Some("one".to_string()),
            fork: Some("two".to_string()),
            continue_latest: false,
            session_picker: false,
            cwd: None,
            model: None,
            mode: None,
            api_key: None,
            base_url: None,
            provider: ProviderKind::Mock,
            prompt: Vec::new(),
        };
        assert_eq!(
            prepare_interactive(request).unwrap_err(),
            "--resume, --fork, and --continue are mutually exclusive"
        );
    }

    #[test]
    fn interactive_launch_preserves_requested_cwd() {
        let cwd = tempfile::tempdir().expect("temporary workspace");
        let request = InteractiveLaunchRequest {
            app_version: "0.2.55".to_string(),
            resume: None,
            fork: None,
            continue_latest: false,
            session_picker: false,
            cwd: Some(cwd.path().to_path_buf()),
            model: None,
            mode: None,
            api_key: Some("test-key".to_string()),
            base_url: None,
            provider: ProviderKind::Mock,
            prompt: vec!["hello".to_string()],
        };

        let config = prepare_interactive(request).expect("interactive config");
        assert_eq!(config.cwd.as_deref(), Some(cwd.path()));
    }

    #[test]
    fn subagent_worker_requires_complete_worktree_identity() {
        assert!(validate_worktree_pair(Some(PathBuf::from("repo")), None).is_err());
        assert!(validate_worktree_pair(None, Some(PathBuf::from("tree"))).is_err());
        assert!(validate_worktree_pair(None, None).unwrap().is_none());
    }
}
