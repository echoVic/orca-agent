use std::path::Path;

use chrono::Local;
use orca_platform::shell::{ShellKind, ShellResolver, ShellSpec};
use orca_tools::schema::{ToolPolicy, canonical_tool_definitions};

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

#[cfg(test)]
pub fn build_system_prompt_with_shell(cwd: &Path, shell: Option<&ShellSpec>) -> String {
    build_system_prompt_with_shell_and_tools(cwd, shell, None)
}

fn build_system_prompt_with_shell_and_tools(
    cwd: &Path,
    shell: Option<&ShellSpec>,
    allowed_tools: Option<&[String]>,
) -> String {
    let tools = render_tool_prompt_section(allowed_tools);
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
- You can mark multiple items complete in a single `update_plan` call.
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

## Available Tools

{tools}

{command_guidance}
{shell_guidance}

When using `web_search` for requests about latest news, recent updates, current status, today, this week, this month, or "最新/最近/今天", include a `fresh_days` value that matches the requested recency instead of relying on the query text alone. Examples: use `fresh_days: 1` for today/current breakage, `fresh_days: 7` for this week/recent updates, and `fresh_days: 30` for latest news or recent releases unless the user asks for a broader range.

## Dynamic workflows

For requests that explicitly ask for a workflow, ultracode, a reusable orchestration, or a multi-phase process whose later phases depend on earlier results, call WorkflowDraft first unless the user already provided an explicit script, scriptPath, name, or draftId. Do not route an ordinary parallel investigation, code review, or handful of independent branches through Workflow; use direct `subagent` calls for those. The draft script must include export const meta with name, description, and phases, and should keep intermediate data inside workflow state/script variables instead of flooding the main conversation. Show the generated preview to the user. Then launch the approved draft with Workflow using draftId, or use Workflow directly for explicit script/scriptPath/name/draftId launches. Saved workflows can be launched with Workflow using name and args.

Valid workflow scripts use exactly one of these shapes:
- Auto mode: `export const meta = {{ name, description, phases: [{{ name, tasks: [{{ prompt: "..." }}] }}] }}`. Every task must be an object with a non-empty `prompt`.
- Hand-written mode: `export const meta = {{ name, description, phases: ["phase-name"] }}` plus top-level `await phase("phase-name", async () => agent("prompt"))`, then `export default result`.

Do not export `run`, `async function run`, helper functions, or arbitrary symbols. The workflow host only supports `export const meta`, `export const phases`, `export const args`, and `export default`.

For delegated work outside Workflow, `subagent` accepts a bounded task and quickly returns its `task_id`. There is no mode parameter. A queued response means the task was accepted; do not submit it again. While it runs, continue independent work. Use `task_wait` when its result is needed, `task_read_output` to retrieve evidence, `subagent_message` for new guidance, and `task_stop` to cancel it. A wait timeout does not stop execution. Workflow runs may set `tokenBudget`; the final notification reports `total`, `spent`, and `remaining`, and each child reserves available run capacity before it starts so concurrent launches cannot claim the same tokens.

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
        tools = tools,
        command_guidance = command_guidance,
    )
}

fn render_command_guidance(shell_allowed: bool) -> &'static str {
    if shell_allowed {
        r#"## Running commands

Every shell command goes through `bash`. Command syntax follows the host shell named in the environment section above, not the tool name.

`bash` starts the command once and then returns. The returned `task_id` is how you keep observing it:

- `state: "running"` means the command is still executing. Continue with the same `task_id`; never start it a second time.
- `yield_time_ms` only controls how long this call waits before handing control back. It never stops the command.
- `timeout_ms` is the only caller-side limit on how long the command may run. Set it only when you actually need an execution deadline.
- `task_read_output` reads more output using the cursor you pass, `task_send_input` types into a `pty` task, `task_wait` blocks until a task changes state or finishes, and `task_stop` stops one.
- Long-running commands notify you when they finish. If you have independent work, keep doing it. When you must have the result, use `task_wait` — do not use `sleep` or repeated short polling to wait for a task you started.

For file inspection, prefer `read_file`, `glob`, and `grep`."#
    } else {
        r#"## Inspecting files

Use only the tools advertised above. This role has no shell or process execution tool. Use `read_file`, `glob`, and `grep` for source inspection and never attempt `bash` or another command tool."#
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

fn render_tool_prompt_section(allowed_tools: Option<&[String]>) -> String {
    let registry = orca_tools::registry::default_tool_registry();
    let mut output = String::new();
    for tool in canonical_tool_definitions(&ToolPolicy::base(), registry)
        .into_iter()
        .filter(|tool| tool_allowed(allowed_tools, &tool.name))
    {
        output.push_str(&format!("\n### {}\n{}\n", tool.name, tool.description));
    }
    output
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
    fn prompt_recommends_glob_and_hides_list_files() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("### glob"));
        assert!(!prompt.contains("### list_files"));
        assert!(prompt.contains("prefer `read_file`, `glob`, and `grep`"));
    }

    #[test]
    fn prompt_does_not_inline_full_tool_json_schemas() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("### update_plan"));
        assert!(!prompt.contains("Parameters: `"));
        assert!(!prompt.contains(r#""additionalProperties":false"#));
        assert!(prompt.contains(r#"{"plan":[{"step":"Inspect code","status":"in_progress"}]}"#));
    }

    #[test]
    fn prompt_names_bash_as_the_single_command_entry_point() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("### bash"));
        assert!(prompt.contains("Every shell command goes through `bash`"));
        assert!(prompt.contains("`task_wait`"));
        assert!(prompt.contains("never start it a second time"));
    }

    #[test]
    fn restricted_prompt_only_advertises_effective_tools() {
        let allowed = vec![
            "read_file".to_string(),
            "glob".to_string(),
            "grep".to_string(),
        ];
        let prompt = build_system_prompt_for_tools(std::path::Path::new("/repo"), Some(&allowed));

        assert!(prompt.contains("### read_file"));
        assert!(prompt.contains("### glob"));
        assert!(prompt.contains("### grep"));
        assert!(!prompt.contains("### bash"));
        assert!(!prompt.contains("Every shell command goes through `bash`"));
        assert!(prompt.contains("This role has no shell or process execution tool"));
    }

    #[test]
    fn prompt_separates_waiting_from_the_execution_deadline() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("`yield_time_ms` only controls how long this call waits"));
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
    fn prompt_routes_dynamic_workflows_through_preview_drafts() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("explicitly ask for a workflow"));
        assert!(prompt.contains("call WorkflowDraft first"));
        assert!(prompt.contains("Do not route an ordinary parallel investigation"));
        assert!(prompt.contains("use direct `subagent` calls"));
        assert!(prompt.contains("Then launch the approved draft with Workflow using draftId"));
        assert!(prompt.contains("Do not export `run`"));
        assert!(prompt.contains("tasks: [{ prompt:"));
        assert!(prompt.contains("quickly returns its `task_id`"));
        assert!(!prompt.contains(r#""mode":"async""#));
        assert!(prompt.contains("do not submit it again"));
    }

    #[test]
    fn prompt_hides_goal_only_tool_from_base_prompt() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(!prompt.contains("### update_goal"));
    }
}
