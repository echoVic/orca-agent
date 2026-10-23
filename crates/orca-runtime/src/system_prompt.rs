use std::path::Path;

use chrono::Local;
use orca_core::config::RunConfig;
use orca_platform::shell::{ShellKind, ShellResolver, ShellSpec};

#[cfg(test)]
pub fn build_system_prompt(cwd: &Path) -> String {
    build_system_prompt_for_tools(cwd, None)
}

pub fn build_system_prompt_for_tools(cwd: &Path, allowed_tools: Option<&[String]>) -> String {
    let shell = ShellResolver::for_current_host()
        .resolve_from_environment()
        .ok();
    build_system_prompt_with_shell_and_tools(cwd, shell.as_ref(), allowed_tools)
}

pub(crate) fn build_system_prompt_for_config(
    cwd: &Path,
    _config: &RunConfig,
    _subagent_depth: u32,
    allowed_tools: Option<&[String]>,
) -> String {
    let shell = ShellResolver::for_current_host()
        .resolve_from_environment()
        .ok();
    build_system_prompt_with_shell_and_tools(cwd, shell.as_ref(), allowed_tools)
}

#[cfg(test)]
pub fn build_system_prompt_with_shell(cwd: &Path, shell: Option<&ShellSpec>) -> String {
    build_system_prompt_with_shell_and_tools(cwd, shell, None)
}

fn build_system_prompt_with_shell_and_tools(
    cwd: &Path,
    shell: Option<&ShellSpec>,
    allowed_tools: Option<&[String]>,
) -> String {
    let git_repository = crate::workspace_git::is_git_work_tree(cwd);
    let git_environment = if git_repository {
        ""
    } else {
        "- Git repository: no\n"
    };
    let shell_allowed = tool_allowed(allowed_tools, "bash");
    let shell_environment = if shell_allowed {
        render_shell_environment(shell)
    } else {
        "Shell tool: unavailable for this role; use the advertised read-only file tools".to_string()
    };
    let command_guidance = render_command_guidance(shell_allowed);
    let shell_guidance = shell_allowed
        .then(|| render_shell_guidance(shell))
        .unwrap_or("");
    format!(
        r#"You are Orca, an expert software engineering agent running in a terminal-based coding assistant. You are precise, safe, and helpful.

## Environment
- Working directory: {cwd}
- Operating system: {os}
- {shell_environment}
- Today's date: {today}
{git_environment}
# How you work

## Personality

Be concise, direct, and friendly — like a teammate handing off work. Communicate efficiently, keeping the user informed about ongoing actions without unnecessary detail. Prioritize actionable guidance over verbose explanations.

## Responsiveness

Before making tool calls, send a brief preamble (1-2 sentences) explaining what you're about to do. For longer tasks, provide short progress updates (8-12 words) at natural milestones. Examples:

- "Explored the repo; now checking the API route definitions."
- "Config looks good. Next up: patching helpers to stay in sync."
- "Tests pass. Wrapping up with a format check."

Exception: skip preambles for trivial reads (e.g., reading a single file) unless part of a grouped action.

## Task execution

Keep going until the task is completely resolved before yielding back to the user. Only end your turn when you are sure the problem is solved. Do NOT guess or make up an answer — use the tools to verify.

When working:
- Read relevant code first. Do not modify code you haven't read.
- Fix problems at the root cause, not with surface-level patches.
- Make minimal, focused changes. Do not refactor unrelated code.
- Do not add comments, type annotations, or docstrings to code you didn't change.
- Keep changes consistent with the existing codebase style.
- Use `git log` and `git blame` if additional history context is needed.
- Do not `git commit` unless explicitly requested.

## Planning

Use `update_plan` to track multi-step work. A plan breaks the task into meaningful, logically ordered steps that are easy to verify. Each step should be 5-7 words max.

Rules:
- Use a plan when the task requires multiple actions or has logical phases.
- Do NOT use a plan for single-step tasks or informational answers.
- After creating a plan, immediately mark the first step `in_progress` and begin executing it. Never stop after just creating the plan.
- Keep exactly one step `in_progress` at all times until done.
- Mark a step `completed` only after verifying it (tests pass, output correct).
- As soon as a step is verified, call `update_plan` before starting the next one: mark it `completed` and the next step `in_progress`. The user follows progress through the plan, so it must not lag behind the work.
- Mark several steps complete in one call only when they finished together; never save plan updates for the end.
- When changing plans mid-task, provide an `explanation` of the rationale.
- Do not repeat the plan contents after calling `update_plan` — the harness already displays it.
- Example tool arguments: `{{"plan":[{{"step":"Inspect code","status":"in_progress"}}]}}`.

**High-quality plan examples:**

1. Add CLI entry with file args
2. Parse Markdown via CommonMark library
3. Apply semantic HTML template
4. Handle code blocks, images, links
5. Add error handling for invalid files

**Low-quality plan examples (avoid):**

1. Create CLI tool
2. Add parser
3. Make it work

## Validating your work

Start validation as specific as possible to the code you changed, then broaden:
- Run the single relevant test first.
- If it passes, run the broader test suite.
- If there's no test and the codebase has tests, add one in the logical location.
- Do not attempt to fix unrelated broken tests.

{command_guidance}
{shell_guidance}

When using `web_search` for requests about latest news, recent updates, current status, today, this week, this month, or "最新/最近/今天", include a `fresh_days` value that matches the requested recency instead of relying on the query text alone. Examples: use `fresh_days: 1` for today/current breakage, `fresh_days: 7` for this week/recent updates, and `fresh_days: 30` for latest news or recent releases unless the user asks for a broader range.

## Dynamic workflows

Explicit workflow/ultracode, reusable orchestration, or dependent phases: WorkflowDraft first unless user supplied script/scriptPath/name/draftId. Show preview; launch approved draft with Workflow(draftId). Ordinary parallel investigation/code review/independent branches: direct `subagent` calls. Keep intermediates in workflow state/script variables. Saved workflows: Workflow(name,args).

## Safety Rules
1. NEVER execute destructive commands (rm -rf /, rm -rf ~, mkfs, dd if=/dev/zero, etc.).
2. NEVER expose, log, or transmit secrets, API keys, passwords, or credentials.
3. NEVER modify files outside the workspace directory.
4. NEVER make network requests to upload or exfiltrate workspace data.
5. If a command could be destructive or irreversible, explain the risk and stop.

## Final response

When done, respond concisely — like a teammate summarizing a PR. Structure your answer only when complexity demands it. For simple results, use plain sentences. Keep it under 10 lines unless the task warrants more detail.

If there's a logical next step you can help with, suggest it briefly."#,
        cwd = cwd.display(),
        os = std::env::consts::OS,
        today = Local::now().format("%Y-%m-%d"),
        command_guidance = command_guidance,
    )
}

fn render_command_guidance(shell_allowed: bool) -> &'static str {
    if shell_allowed {
        r#"## Running commands

Every shell command goes through `bash`. Command syntax follows the host shell named in the environment section above, not the tool name.

`bash` starts the command once and then returns. The returned `task_id` is how you keep observing it:

- Ordinary pipe commands wait up to 10 seconds by default and return sooner when they finish. PTY commands default to 1 second so interactive input stays responsive.
- `state: "running"` means the command is still executing. Continue with the same `task_id`; never start it a second time.
- `yield_time_ms` only controls how long this call waits before handing control back. It never stops the command.
- `timeout_ms` is the only caller-side limit on how long the command may run. Set it only when you actually need an execution deadline.
- `task_read_output` reads more output using the cursor you pass, `task_send_input` types into a `pty` task, `task_wait` blocks until a task changes state or finishes, and `task_stop` stops one.
- Long-running commands notify you when they finish. If you have independent work, keep doing it. When you must have the result, use `task_wait` — do not use `sleep` or repeated short polling to wait for a task you started.

For file inspection, prefer `read_file`, `glob`, and `grep`."#
    } else {
        r#"## Inspecting files

Use only the tools provided for this role. This role has no shell or process execution tool. Use `read_file`, `glob`, and `grep` for source inspection and never attempt `bash` or another command tool."#
    }
}

fn render_shell_environment(shell: Option<&ShellSpec>) -> String {
    match shell {
        Some(shell) => format!(
            "Active shell: {} ({})",
            shell.tool_name(),
            shell.prompt_dialect()
        ),
        None => "Active shell: unavailable (prefer dedicated file tools over shell commands)"
            .to_string(),
    }
}

fn render_shell_guidance(shell: Option<&ShellSpec>) -> &'static str {
    if shell.is_some_and(|shell| matches!(shell.kind(), ShellKind::PowerShell(_) | ShellKind::Cmd))
    {
        "On native Windows shells, Unix utilities are not guaranteed to exist. Do not assume `grep`, `head`, `tail`, `sed`, `awk`, or `find` are available. Use the dedicated file tools, and use the active shell's quoting, chaining, path, and environment-variable syntax."
    } else {
        ""
    }
}

fn tool_allowed(allowed_tools: Option<&[String]>, name: &str) -> bool {
    allowed_tools.is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_platform::host::{Architecture, HostPlatform, OperatingSystem};
    use orca_platform::shell::ShellResolver;
    use std::path::PathBuf;

    fn windows_shell(available: &'static str) -> orca_platform::shell::ShellSpec {
        ShellResolver::new(
            HostPlatform::new(OperatingSystem::Windows, Architecture::X86_64),
            move |name| (name == available).then(|| PathBuf::from(available)),
        )
        .resolve(None)
        .expect("resolve Windows shell")
    }

    #[test]
    fn non_repo_prompt_hides_git_status_and_reports_environment() {
        let cwd = tempfile::tempdir().unwrap();
        assert!(cwd.path().ancestors().all(|dir| !dir.join(".git").exists()));
        let prompt = build_system_prompt_with_shell(cwd.path(), None);

        assert!(!prompt.contains("### git_status\n"));
        let environment = prompt.split("# How you work").next().unwrap();
        assert!(environment.contains("- Git repository: no\n"));
    }

    #[test]
    fn repo_prompt_preserves_existing_text_except_non_repo_git_context() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let non_repo = tempfile::tempdir().unwrap();
        assert!(
            non_repo
                .path()
                .ancestors()
                .all(|dir| !dir.join(".git").exists())
        );
        let repo_prompt = build_system_prompt_with_shell(repo.path(), None);
        let non_repo_prompt = build_system_prompt_with_shell(non_repo.path(), None);

        assert!(!repo_prompt.contains("### git_status\n"));
        assert!(!repo_prompt.contains("Git repository"));
        assert_eq!(
            repo_prompt.replace(&repo.path().display().to_string(), "<cwd>"),
            non_repo_prompt
                .replace(&non_repo.path().display().to_string(), "<cwd>")
                .replace("- Git repository: no\n", "")
        );
    }

    #[test]
    fn prompt_recommends_glob_without_repeating_the_tool_catalog() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(!prompt.contains("## Available Tools"));
        assert!(prompt.contains("prefer `read_file`, `glob`, and `grep`"));
    }

    #[test]
    fn prompt_does_not_inline_full_tool_json_schemas() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(!prompt.lines().any(|line| line.starts_with("### ")));
        assert!(!prompt.contains("Parameters: `"));
        assert!(!prompt.contains(r#""additionalProperties":false"#));
        assert!(prompt.contains(r#"{"plan":[{"step":"Inspect code","status":"in_progress"}]}"#));
    }

    #[test]
    fn prompt_asks_for_a_plan_update_as_each_step_finishes() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains(
            "As soon as a step is verified, call `update_plan` before starting the next one"
        ));
        assert!(!prompt.contains("You can mark multiple items complete in a single"));
    }

    #[test]
    fn prompt_names_bash_as_the_single_command_entry_point() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(!prompt.contains("### bash"));
        assert!(prompt.contains("Every shell command goes through `bash`"));
        assert!(prompt.contains("`task_wait`"));
        assert!(prompt.contains("never start it a second time"));
    }

    #[test]
    fn restricted_prompt_preserves_no_shell_guidance_without_tool_listing() {
        let allowed = vec![
            "read_file".to_string(),
            "glob".to_string(),
            "grep".to_string(),
        ];
        let prompt = build_system_prompt_for_tools(std::path::Path::new("/repo"), Some(&allowed));

        assert!(!prompt.lines().any(|line| line.starts_with("### ")));
        assert!(!prompt.contains("advertised above"));
        assert!(!prompt.contains("### bash"));
        assert!(!prompt.contains("Every shell command goes through `bash`"));
        assert!(prompt.contains("This role has no shell or process execution tool"));
    }

    #[test]
    fn prompt_separates_waiting_from_the_execution_deadline() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("`yield_time_ms` only controls how long this call waits"));
        assert!(prompt.contains("Ordinary pipe commands wait up to 10 seconds by default"));
        assert!(prompt.contains("`timeout_ms` is the only caller-side limit"));
        assert!(prompt.contains("do not use `sleep` or repeated short polling"));
    }

    #[test]
    fn prompt_names_powershell_7_as_the_active_shell_dialect() {
        let prompt = build_system_prompt_with_shell(
            std::path::Path::new(r"C:\repo"),
            Some(&windows_shell("pwsh.exe")),
        );

        assert!(prompt.contains("Active shell: powershell"));
        assert!(prompt.contains("PowerShell 7 syntax"));
        assert!(
            prompt
                .contains("Command syntax follows the host shell named in the environment section")
        );
        assert!(prompt.contains("Unix utilities are not guaranteed"));
    }

    #[test]
    fn prompt_warns_against_powershell_7_operators_on_windows_powershell() {
        let prompt = build_system_prompt_with_shell(
            std::path::Path::new(r"C:\repo"),
            Some(&windows_shell("powershell.exe")),
        );

        assert!(prompt.contains("Windows PowerShell 5.1 syntax"));
        assert!(prompt.contains("Do not use PowerShell 7-only operators such as && or ||"));
    }

    #[test]
    fn prompt_names_cmd_as_the_active_shell_dialect() {
        let prompt = build_system_prompt_with_shell(
            std::path::Path::new(r"C:\repo"),
            Some(&windows_shell("cmd.exe")),
        );

        assert!(prompt.contains("Active shell: cmd"));
        assert!(prompt.contains("cmd.exe syntax"));
        assert!(prompt.contains("cmd quoting rules"));
    }

    #[test]
    fn prompt_requires_fresh_days_for_recent_web_searches() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("include a `fresh_days` value"));
        assert!(prompt.contains("fresh_days: 30"));
        assert!(prompt.contains("最新/最近/今天"));
    }

    #[test]
    fn default_request_prefix_stays_within_the_reviewed_character_budget() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().unwrap();
        let mut config = orca_core::config::RunConfig {
            cwd: Some(cwd.path().to_path_buf()),
            approval_mode: orca_core::approval_types::ApprovalMode::FullAuto,
            ..Default::default()
        };
        config.tools.headless_exec = true;
        let prompt = crate::agent_common::build_agent_system_prompt_for_config(
            cwd.path(),
            &config,
            0,
            &orca_core::subagent_types::SubagentType::General,
            None,
            None,
            None,
        );
        let provider = crate::tool_invocation::provider_config_for_agent_loop(
            &config,
            cwd.path(),
            0,
            &orca_core::subagent_types::SubagentType::General,
            crate::tool_invocation::AgentToolPolicyContext::unrestricted(),
            &orca_mcp::McpRegistry::default(),
            &crate::extension::ExtensionData::new("prefix-budget"),
        );
        let tools =
            orca_provider::tool_schema::deepseek_tools_schema(&provider.tools_override.unwrap());
        let prefix =
            prompt.chars().count() + serde_json::to_string(&tools).unwrap().chars().count();
        // The measured #143 prefix was 32.9k chars. Leave room for request
        // framing while protecting a further 15% reduction in total size.
        assert!(
            prefix <= 27_800,
            "system + tool prefix is {prefix} chars (limit 27800)"
        );
    }

    #[test]
    fn prompt_routes_dynamic_workflows_through_preview_drafts() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        for rule in [
            "Explicit workflow/ultracode",
            "dependent phases",
            "WorkflowDraft first",
            "unless user supplied script/scriptPath/name/draftId",
            "Show preview",
            "approved draft with Workflow(draftId)",
            "Ordinary parallel investigation/code review/independent branches: direct `subagent` calls",
            "Keep intermediates in workflow state/script variables",
            "Workflow(name,args)",
        ] {
            assert!(
                prompt.contains(rule),
                "missing workflow routing rule: {rule}"
            );
        }
        // Script syntax belongs to the tool used to write it, submission
        // mechanics to subagent; the system prompt only selects the route.
        let registry = orca_tools::registry::default_tool_registry();
        let workflow = registry.get("Workflow").unwrap();
        let script = workflow.spec().input_schema["properties"]["script"]["description"]
            .as_str()
            .unwrap();
        assert!(script.contains("Do not export `run`"));
        assert!(script.contains("tasks: [{ prompt:"));
        assert!(script.contains("non-empty prompt"));
        assert!(script.contains("await phase("));
        let subagent = registry.get("subagent").unwrap();
        assert!(subagent.description().contains("quickly returns task_id"));
        assert!(subagent.description().contains("No mode parameter"));
        assert!(subagent.description().contains("do not resubmit"));
    }

    #[test]
    fn prompt_hides_goal_only_tool_from_base_prompt() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(!prompt.contains("### update_goal"));
    }
}
