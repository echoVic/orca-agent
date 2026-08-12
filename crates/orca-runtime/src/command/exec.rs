use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::file::ConfigOverrides;
use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};

use super::config::{RunConfigRequest, build_run_config};

#[derive(Clone, Debug)]
pub struct ExecCommandRequest {
    pub app_version: String,
    pub output_format: OutputFormat,
    pub cwd: Option<PathBuf>,
    pub approval_mode: Option<ApprovalMode>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub verifier: Option<String>,
    pub max_budget: Option<f64>,
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_time_secs: Option<u64>,
    pub resume: Option<String>,
    pub resume_at: Option<String>,
    pub fork: Option<String>,
    pub continue_latest: bool,
    pub no_history: bool,
    pub save_history: bool,
    pub provider: ProviderKind,
    pub prompt: Vec<String>,
}

pub fn run(request: ExecCommandRequest) -> i32 {
    let stdin = io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    run_with_stdin(request, stdin_is_terminal, stdin)
}

pub fn run_with_stdin(
    request: ExecCommandRequest,
    stdin_is_terminal: bool,
    stdin: impl Read,
) -> i32 {
    if request.no_history
        && (request.resume.is_some() || request.fork.is_some() || request.continue_latest)
    {
        eprintln!("orca: --resume/--fork/--continue cannot be combined with --no-history");
        return 1;
    }
    if request.no_history && request.save_history {
        eprintln!("orca: --save-history cannot be combined with --no-history");
        return 1;
    }
    let resume_like = request.resume.is_some() as u8
        + request.fork.is_some() as u8
        + request.continue_latest as u8;
    if resume_like > 1 {
        eprintln!("orca: --resume, --fork, and --continue are mutually exclusive");
        return 1;
    }
    if request.resume_at.is_some() && request.fork.is_some() {
        eprintln!("orca: --resume-at cannot be combined with --fork");
        return 1;
    }
    if request.resume_at.is_some() && request.resume.is_none() && !request.continue_latest {
        eprintln!("orca: --resume-at requires --resume, --continue, or the resume subcommand");
        return 1;
    }

    let prompt = match resolve_prompt(request.prompt, stdin_is_terminal, stdin) {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let config_cwd = request
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let fallback = if request.no_history
        || (request.output_format == OutputFormat::Jsonl && !request.save_history)
    {
        HistoryMode::Disabled
    } else {
        HistoryMode::Record
    };
    let history_mode = resolve_history_mode(
        request.resume,
        request.fork,
        request.continue_latest,
        request.resume_at,
        fallback,
    );
    let mut config_request = RunConfigRequest::new(request.app_version, config_cwd);
    config_request.runtime_cwd = request.cwd;
    config_request.prompt = prompt;
    config_request.output_format = request.output_format;
    config_request.provider = request.provider;
    config_request.verifier = request.verifier;
    config_request.history_mode = history_mode;
    config_request.budget = orca_core::config::BudgetConfig {
        max_turns: request.max_turns,
        max_tool_calls: request.max_tool_calls,
        max_cost_usd_micros: request
            .max_cost_usd
            .or(request.max_budget)
            .filter(|usd| usd.is_finite() && *usd > 0.0)
            .map(|usd| (usd * 1_000_000.0).round() as u64),
        max_wall_time_ms: request
            .max_wall_time_secs
            .map(|secs| secs.saturating_mul(1_000)),
    };
    config_request.overrides = ConfigOverrides {
        model: request.model,
        mode: request.approval_mode,
        api_key: request.api_key,
        base_url: request.base_url,
        reasoning_effort: None,
    };

    match build_run_config(config_request) {
        Ok(config) => crate::controller::run(config),
        Err(error) => {
            eprintln!("orca: {error}");
            1
        }
    }
}

pub fn resolve_prompt(
    prompt_args: Vec<String>,
    stdin_is_terminal: bool,
    mut stdin: impl Read,
) -> Result<String, String> {
    let force_stdin = prompt_args.len() == 1 && prompt_args[0] == "-";
    let has_prompt = !prompt_args.is_empty() && !force_stdin;
    let prompt = if has_prompt {
        prompt_args.join(" ")
    } else {
        String::new()
    };

    if force_stdin || !has_prompt {
        if stdin_is_terminal {
            return Err(
                "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
                    .to_string(),
            );
        }
        let stdin_text = read_stdin_text(&mut stdin)?;
        if stdin_text.trim().is_empty() {
            return Err("No prompt provided via stdin.".to_string());
        }
        return Ok(stdin_text);
    }

    if stdin_is_terminal {
        return Ok(prompt);
    }
    let stdin_text = read_stdin_text(&mut stdin)?;
    if stdin_text.trim().is_empty() {
        Ok(prompt)
    } else {
        Ok(prompt_with_stdin_context(&prompt, &stdin_text))
    }
}

fn read_stdin_text(reader: &mut impl Read) -> Result<String, String> {
    let mut buffer = String::new();
    reader
        .read_to_string(&mut buffer)
        .map_err(|error| format!("failed to read stdin: {error}"))?;
    Ok(buffer)
}

fn prompt_with_stdin_context(prompt: &str, stdin_text: &str) -> String {
    let mut combined = format!("{prompt}\n\n<stdin>\n{stdin_text}");
    if !stdin_text.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str("</stdin>");
    combined
}

pub(crate) fn resolve_history_mode(
    resume: Option<String>,
    fork: Option<String>,
    continue_latest: bool,
    resume_at: Option<String>,
    fallback: HistoryMode,
) -> HistoryMode {
    if let Some(selector) = fork {
        HistoryMode::Fork(selector)
    } else if let Some(resume_at) = resume_at {
        let selector = resume.or_else(|| {
            if continue_latest {
                Some("latest".to_string())
            } else {
                None
            }
        });
        match selector {
            Some(selector) => HistoryMode::ResumeAt {
                selector,
                resume_at,
            },
            None => fallback,
        }
    } else if let Some(selector) = resume.or_else(|| {
        if continue_latest {
            Some("latest".to_string())
        } else {
            None
        }
    }) {
        HistoryMode::Resume(selector)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn resolves_argument_prompt_without_reading_terminal_stdin() {
        assert_eq!(
            resolve_prompt(vec!["inspect".into(), "repo".into()], true, io::empty()).unwrap(),
            "inspect repo"
        );
    }

    #[test]
    fn resolves_dash_and_omitted_prompts_from_piped_stdin() {
        assert_eq!(
            resolve_prompt(vec!["-".into()], false, io::Cursor::new("from pipe\n")).unwrap(),
            "from pipe\n"
        );
        assert_eq!(
            resolve_prompt(Vec::new(), false, io::Cursor::new("from pipe")).unwrap(),
            "from pipe"
        );
    }

    #[test]
    fn appends_nonempty_piped_stdin_to_argument_prompt() {
        assert_eq!(
            resolve_prompt(vec!["review".into()], false, io::Cursor::new("context")).unwrap(),
            "review\n\n<stdin>\ncontext\n</stdin>"
        );
    }

    #[test]
    fn rejects_missing_or_empty_piped_prompt() {
        assert_eq!(
            resolve_prompt(Vec::new(), true, io::empty()).unwrap_err(),
            "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
        );
        assert_eq!(
            resolve_prompt(Vec::new(), false, io::Cursor::new("  \n")).unwrap_err(),
            "No prompt provided via stdin."
        );
    }
}
