#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    New,
    Model(Option<String>),
    Compact,
    Recap,
    Resume,
    Fork(Option<String>),
    Side(Option<String>),
    Rename(Option<String>),
    Status,
    Copy(Option<String>),
    CancelOperation,
    Cost,
    Config,
    Mcp,
    Mode(Option<String>),
    Plan(Option<String>),
    Goal(GoalSlashCommand),
    Queue(QueueSlashCommand),
    WorkflowList,
    WorkflowRun {
        name: String,
        args: Option<String>,
    },
    AgentDashboard,
    TaskWorkspace,
    TaskFollowUp {
        task_id: String,
        prompt: String,
    },
    Remember(String),
    SkillList,
    SkillRun {
        id: String,
        args: Option<String>,
    },
    /// `/mcp__{server}__{prompt} args…`, a prompt in the MCP catalog, with
    /// the text after its name.
    McpPrompt {
        server: String,
        prompt: String,
        args: String,
    },
    Trust(TrustSlashCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustSlashCommand {
    Show,
    Add,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalSlashCommand {
    Show,
    Set(String),
    Edit(String),
    Clear,
    Pause,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueSlashCommand {
    List,
    Pause,
    Start,
}

pub fn parse(input: &str) -> Option<SlashCommand> {
    parse_static(input)
}

pub(crate) fn parse_with_cwd(
    input: &str,
    cwd: &Path,
    mcp_prompts: &[McpPromptView],
) -> Option<SlashCommand> {
    if let Some(command) = parse_static(input) {
        return Some(command);
    }

    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next()?;
    if builtin_command_names().contains(command) {
        return None;
    }
    // A prompt's last argument takes the rest of the text as typed.
    if let Some(prompt) = find_mcp_prompt(mcp_prompts, command) {
        return Some(SlashCommand::McpPrompt {
            server: prompt.server.clone(),
            prompt: prompt.name.clone(),
            args: rest.trim_start()[command.len()..].trim().to_string(),
        });
    }
    let args = parts.collect::<Vec<_>>().join(" ");
    let args_opt = if args.is_empty() { None } else { Some(args) };

    // saved workflow takes priority over skill
    if let Some(saved_workflow) = discover_saved_workflows(cwd)
        .into_iter()
        .map(|(name, _)| name)
        .find(|name| name == command)
    {
        return Some(SlashCommand::WorkflowRun {
            name: saved_workflow,
            args: args_opt,
        });
    }

    // skill alias: /skill-id [args]
    if let Ok(skills) = orca_tools::skills::discover_from_env(cwd) {
        if skills.iter().any(|s| s.id == command) {
            return Some(SlashCommand::SkillRun {
                id: command.to_string(),
                args: args_opt,
            });
        }
    }

    None
}

/// Why `input`, a slash command that did not parse, was not run: how to
/// write a built-in command given the wrong arguments, or that there is no
/// such command.
pub fn invalid_slash_command_message(input: &str) -> String {
    let command = input.trim().split_whitespace().next().unwrap_or("/");
    let usage = match command {
        "/workflow" => "/workflow:<name> [args]",
        "/model" => "/model or /model <name>",
        "/mode" => "/mode or /mode suggest|auto-edit|full-auto|plan",
        "/plan" => "/plan or /plan off",
        "/queue" => "/queue, /queue pause, or /queue start",
        "/trust" => "/trust, /trust add, or /trust remove",
        "/remember" => "/remember <note>",
        "/goal" => "/goal edit <objective>",
        "/task-follow-up" => "/task-follow-up <task-id> <message>",
        _ if builtin_command_names().contains(command.trim_start_matches('/')) => {
            return format!("{command} takes no arguments.");
        }
        _ => {
            return format!(
                "unknown slash command `{command}`. Type / to view available commands."
            );
        }
    };
    format!("Use {usage}.")
}

fn parse_static(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next()?;
    match command {
        "new" | "clear" => no_arguments(parts).then_some(SlashCommand::New),
        "model" => optional_single_argument(parts)
            .map(|model| SlashCommand::Model(model.map(str::to_string))),
        "compact" => no_arguments(parts).then_some(SlashCommand::Compact),
        "recap" => no_arguments(parts).then_some(SlashCommand::Recap),
        "resume" => no_arguments(parts).then_some(SlashCommand::Resume),
        "fork" => Some(SlashCommand::Fork(optional_argument(parts))),
        "side" => Some(SlashCommand::Side(optional_argument(parts))),
        "rename" => Some(SlashCommand::Rename(optional_argument(parts))),
        "status" => no_arguments(parts).then_some(SlashCommand::Status),
        "copy" => Some(SlashCommand::Copy(optional_argument(parts))),
        "cancel-operation" => no_arguments(parts).then_some(SlashCommand::CancelOperation),
        "cost" => no_arguments(parts).then_some(SlashCommand::Cost),
        "config" => no_arguments(parts).then_some(SlashCommand::Config),
        "mcp" => no_arguments(parts).then_some(SlashCommand::Mcp),
        "mode" => {
            optional_single_argument(parts).map(|mode| SlashCommand::Mode(mode.map(str::to_string)))
        }
        "plan" => {
            optional_single_argument(parts).map(|arg| SlashCommand::Plan(arg.map(str::to_string)))
        }
        "goal" => parse_goal(parts.collect::<Vec<_>>().join(" ")).map(SlashCommand::Goal),
        "queue" => match optional_single_argument(parts)? {
            None | Some("list") => Some(SlashCommand::Queue(QueueSlashCommand::List)),
            Some("pause") => Some(SlashCommand::Queue(QueueSlashCommand::Pause)),
            Some("start") => Some(SlashCommand::Queue(QueueSlashCommand::Start)),
            Some(_) => None,
        },
        command if command.starts_with("workflow:") => {
            let name = command.trim_start_matches("workflow:").trim();
            if name.is_empty() {
                None
            } else {
                let args = parts.collect::<Vec<_>>().join(" ");
                Some(SlashCommand::WorkflowRun {
                    name: name.to_string(),
                    args: if args.is_empty() { None } else { Some(args) },
                })
            }
        }
        "workflows" => no_arguments(parts).then_some(SlashCommand::WorkflowList),
        "agents" => no_arguments(parts).then_some(SlashCommand::AgentDashboard),
        "tasks" => no_arguments(parts).then_some(SlashCommand::TaskWorkspace),
        "task-follow-up" => {
            let task_id = parts.next()?.to_string();
            let prompt = parts.collect::<Vec<_>>().join(" ");
            (!prompt.trim().is_empty()).then_some(SlashCommand::TaskFollowUp { task_id, prompt })
        }
        "skills" => no_arguments(parts).then_some(SlashCommand::SkillList),
        "remember" => {
            let note = parts.collect::<Vec<_>>().join(" ");
            if note.is_empty() {
                None
            } else {
                Some(SlashCommand::Remember(note))
            }
        }
        "trust" => Some(SlashCommand::Trust(
            match optional_single_argument(parts)? {
                None | Some("show") => TrustSlashCommand::Show,
                Some("add") => TrustSlashCommand::Add,
                Some("remove") => TrustSlashCommand::Remove,
                Some(_) => return None,
            },
        )),
        _ => None,
    }
}

pub fn all_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("/new", "Start a new conversation"),
        ("/model", "Switch model and reasoning effort"),
        ("/compact", "Compress conversation context"),
        ("/recap", "Recap what this conversation has done so far"),
        ("/resume", "Resume a saved conversation"),
        ("/fork", "Fork this conversation"),
        ("/side", "Ask without disrupting the main conversation"),
        ("/rename", "Rename this conversation"),
        ("/status", "Show session status"),
        ("/copy", "Copy an assistant response"),
        ("/cancel-operation", "Cancel a recoverable operation"),
        ("/cost", "Show session cost"),
        ("/config", "Configure runtime settings"),
        ("/mcp", "Manage MCP servers"),
        ("/mode", "Switch approval mode"),
        ("/plan", "Plan first, then approve implementation"),
        ("/goal", "Manage a persistent goal"),
        ("/queue", "List, pause, or start queued prompts"),
        ("/workflow:<name>", "Run a saved workflow"),
        ("/workflows", "Show workflow tasks"),
        ("/agents", "Open Agent Workspace"),
        ("/tasks", "Toggle the tasks dock"),
        ("/skills", "Browse and insert a skill"),
        ("/remember", "Save a note to memory"),
        ("/trust", "Manage folder trust for the OS sandbox"),
    ]
}

fn optional_argument<'a>(parts: impl Iterator<Item = &'a str>) -> Option<String> {
    let argument = parts.collect::<Vec<_>>().join(" ");
    (!argument.is_empty()).then_some(argument)
}

fn optional_single_argument<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Option<Option<&'a str>> {
    let argument = parts.next();
    parts.next().is_none().then_some(argument)
}

fn no_arguments<'a>(mut parts: impl Iterator<Item = &'a str>) -> bool {
    parts.next().is_none()
}

pub(crate) fn available_commands(
    cwd: &Path,
    mcp_prompts: &[McpPromptView],
) -> Vec<(String, String)> {
    let mut commands = all_commands()
        .iter()
        .map(|(command, description)| ((*command).to_string(), (*description).to_string()))
        .collect::<Vec<_>>();
    for (name, scope) in discover_saved_workflows(cwd) {
        commands.push((
            format!("/workflow:{name}"),
            format!("Run saved {scope} workflow"),
        ));
        if !builtin_command_names().contains(name.as_str()) {
            commands.push((format!("/{name}"), format!("Run saved {scope} workflow")));
        }
    }
    if let Ok(skills) = orca_tools::skills::discover_from_env(cwd) {
        for skill in skills {
            let desc = if skill.description.is_empty() {
                skill.name.clone()
            } else {
                skill.description.clone()
            };
            if !builtin_command_names().contains(skill.id.as_str()) {
                commands.push((format!("/{}", skill.id), format!("Run skill: {desc}")));
            }
        }
    }
    commands.extend(
        mcp_prompts
            .iter()
            .filter(|prompt| can_type_mcp_prompt(prompt))
            .map(|prompt| {
                (
                    mcp_prompt_command(&prompt.server, &prompt.name),
                    mcp_prompt_menu_description(prompt),
                )
            }),
    );
    commands
}

/// The slash command that runs the catalog prompt `prompt` of the MCP
/// server the catalog names `server`: `/mcp__{server}__{prompt}`, named as
/// the server's tools are.
pub(crate) fn mcp_prompt_command(server: &str, prompt: &str) -> String {
    format!("/mcp__{server}__{prompt}")
}

/// The catalog prompt the slash command `/{word}` runs, `word` being
/// `mcp__{server}__{prompt}`.
pub(crate) fn find_mcp_prompt<'a>(
    prompts: &'a [McpPromptView],
    word: &str,
) -> Option<&'a McpPromptView> {
    let rest = word.strip_prefix("mcp__")?;
    prompts.iter().find(|prompt| {
        can_type_mcp_prompt(prompt)
            && rest
                .strip_prefix(prompt.server.as_str())
                .and_then(|name| name.strip_prefix("__"))
                == Some(prompt.name.as_str())
    })
}

/// A prompt whose name holds whitespace cannot be typed as one word, so no
/// slash command runs it.
fn can_type_mcp_prompt(prompt: &McpPromptView) -> bool {
    !prompt.name.contains(char::is_whitespace)
}

/// `prompt`'s arguments as its usage shows them, in order: `<required>`,
/// `[optional]`.
fn mcp_prompt_argument_hint(prompt: &McpPromptView) -> String {
    prompt
        .arguments
        .iter()
        .map(|(name, required)| {
            if *required {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How the slash menu describes `prompt`: its description, on one line,
/// then its arguments.
fn mcp_prompt_menu_description(prompt: &McpPromptView) -> String {
    let description = prompt
        .description
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let description = if description.is_empty() {
        "Run MCP prompt".to_string()
    } else {
        description
    };
    [description, mcp_prompt_argument_hint(prompt)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `usage: /mcp__{server}__{prompt} <required> [optional]`.
fn mcp_prompt_usage(prompt: &McpPromptView) -> String {
    [
        format!(
            "usage: {}",
            mcp_prompt_command(&prompt.server, &prompt.name)
        ),
        mcp_prompt_argument_hint(prompt),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn builtin_command_names() -> std::collections::BTreeSet<&'static str> {
    let mut names = all_commands()
        .iter()
        .filter_map(|(command, _)| {
            command
                .strip_prefix('/')
                .and_then(|name| name.split([' ', ':']).next())
        })
        .collect::<std::collections::BTreeSet<_>>();
    names.insert("clear");
    names
}

fn discover_saved_workflows(cwd: &Path) -> Vec<(String, &'static str)> {
    let mut workflows = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for ancestor in cwd.ancestors() {
        collect_workflow_dir(
            &ancestor.join(".orca").join("workflows"),
            "project",
            &mut seen,
            &mut workflows,
        );
    }
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        collect_workflow_dir(
            &home.join(".orca").join("workflows"),
            "user",
            &mut seen,
            &mut workflows,
        );
    }
    workflows
}

fn collect_workflow_dir(
    dir: &Path,
    scope: &'static str,
    seen: &mut std::collections::BTreeSet<String>,
    workflows: &mut Vec<(String, &'static str)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
                return None;
            }
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        if seen.insert(name.clone()) {
            workflows.push((name, scope));
        }
    }
}

fn parse_goal(args: String) -> Option<GoalSlashCommand> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Some(GoalSlashCommand::Show);
    }
    match trimmed {
        "clear" => Some(GoalSlashCommand::Clear),
        "pause" => Some(GoalSlashCommand::Pause),
        "resume" => Some(GoalSlashCommand::Resume),
        "edit" => None,
        _ => {
            if let Some(rest) = trimmed.strip_prefix("edit ") {
                let objective = rest.trim();
                if objective.is_empty() {
                    None
                } else {
                    Some(GoalSlashCommand::Edit(objective.to_string()))
                }
            } else {
                Some(GoalSlashCommand::Set(trimmed.to_string()))
            }
        }
    }
}

/// The arguments (name, value) that `args`, the text after
/// `/mcp__{server}__{prompt}`, gives `prompt`: its words, split at
/// whitespace, go to the declared arguments in order, and the last argument
/// takes all the text that is left. An optional argument with no text is
/// left out. A required argument with no text, or text for a prompt that
/// takes no arguments, is answered with the prompt's usage.
pub(crate) fn map_prompt_arguments(
    prompt: &McpPromptView,
    args: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut rest = args.trim();
    let Some(last) = prompt.arguments.len().checked_sub(1) else {
        return if rest.is_empty() {
            Ok(Vec::new())
        } else {
            Err(mcp_prompt_usage(prompt))
        };
    };
    let mut arguments = Vec::new();
    for (index, (name, required)) in prompt.arguments.iter().enumerate() {
        let value = if index == last {
            std::mem::take(&mut rest)
        } else {
            let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            rest = tail.trim_start();
            word
        };
        if !value.is_empty() {
            arguments.push((name.clone(), value.to_string()));
        } else if *required {
            return Err(mcp_prompt_usage(prompt));
        }
    }
    Ok(arguments)
}

pub fn available_models() -> &'static [&'static str] {
    orca_core::model::preset_models()
}

pub fn validate_model(model: &str) -> Result<(), String> {
    orca_core::model::validate_model(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_command() {
        assert_eq!(
            parse("/model deepseek-v4-pro"),
            Some(SlashCommand::Model(Some("deepseek-v4-pro".to_string())))
        );
        assert_eq!(parse("/model"), Some(SlashCommand::Model(None)));
    }

    #[test]
    fn parses_mode_commands() {
        assert_eq!(parse("/mode"), Some(SlashCommand::Mode(None)));
        assert_eq!(
            parse("/mode auto-edit"),
            Some(SlashCommand::Mode(Some("auto-edit".to_string())))
        );
    }

    #[test]
    fn parses_plan_commands() {
        assert_eq!(parse("/plan"), Some(SlashCommand::Plan(None)));
        assert_eq!(
            parse("/plan off"),
            Some(SlashCommand::Plan(Some("off".to_string())))
        );
    }

    #[test]
    fn fixed_arity_commands_reject_trailing_arguments() {
        for command in [
            "/new now",
            "/clear now",
            "/model auto extra",
            "/compact now",
            "/resume latest",
            "/status now",
            "/cancel-operation now",
            "/cost now",
            "/config show",
            "/mcp now",
            "/mode auto-edit extra",
            "/plan off extra",
            "/workflows now",
            "/agents now",
            "/skills now",
            "/trust add extra",
        ] {
            assert_eq!(parse(command), None, "accepted malformed {command}");
        }
    }

    #[test]
    fn resume_is_the_only_saved_session_slash_command() {
        assert!(parse("/resume").is_some());
        assert_eq!(parse("/history"), None);
        let commands = all_commands();
        assert_eq!(
            commands
                .iter()
                .find(|(command, _)| *command == "/resume")
                .map(|(_, description)| *description),
            Some("Resume a saved conversation")
        );
        assert!(!commands.iter().any(|(command, _)| *command == "/history"));
    }

    #[test]
    fn new_and_clear_resolve_to_the_same_new_conversation_command() {
        assert_eq!(parse("/new"), Some(SlashCommand::New));
        assert_eq!(parse("/clear"), Some(SlashCommand::New));
    }

    #[test]
    fn slash_menu_exposes_new_without_duplicate_clear_alias() {
        let commands = all_commands();
        assert_eq!(
            commands
                .iter()
                .find(|(command, _)| *command == "/new")
                .map(|(_, description)| *description),
            Some("Start a new conversation")
        );
        assert!(!commands.iter().any(|(command, _)| *command == "/clear"));
    }

    #[test]
    fn clear_alias_is_reserved_from_dynamic_command_collisions() {
        assert!(builtin_command_names().contains("clear"));
    }

    #[test]
    fn a_known_command_with_the_wrong_arguments_says_how_to_write_it() {
        // `/recap now` said "unknown slash command `/recap`".
        assert_eq!(
            invalid_slash_command_message("/recap now"),
            "/recap takes no arguments."
        );
        assert_eq!(
            invalid_slash_command_message("/workflows all"),
            "/workflows takes no arguments."
        );
        assert_eq!(
            invalid_slash_command_message("/model a b"),
            "Use /model or /model <name>."
        );
        assert_eq!(
            invalid_slash_command_message("/queue clear"),
            "Use /queue, /queue pause, or /queue start."
        );
        assert_eq!(
            invalid_slash_command_message("/trust everything"),
            "Use /trust, /trust add, or /trust remove."
        );
        assert_eq!(
            invalid_slash_command_message("/remember"),
            "Use /remember <note>."
        );
        assert_eq!(
            invalid_slash_command_message("/goal edit"),
            "Use /goal edit <objective>."
        );
        assert!(
            invalid_slash_command_message("/recapp").contains("unknown slash command `/recapp`")
        );
    }

    #[test]
    fn parses_operation_cancellation_command() {
        assert_eq!(
            parse("/cancel-operation"),
            Some(SlashCommand::CancelOperation)
        );
    }

    #[test]
    fn parses_session_lifecycle_commands_with_optional_arguments() {
        assert_eq!(parse("/fork"), Some(SlashCommand::Fork(None)));
        assert_eq!(
            parse("/fork auth experiment"),
            Some(SlashCommand::Fork(Some("auth experiment".to_string())))
        );
        assert_eq!(parse("/rename"), Some(SlashCommand::Rename(None)));
        assert_eq!(
            parse("/rename release triage"),
            Some(SlashCommand::Rename(Some("release triage".to_string())))
        );
        assert_eq!(parse("/status"), Some(SlashCommand::Status));
        assert_eq!(parse("/copy"), Some(SlashCommand::Copy(None)));
        assert_eq!(
            parse("/copy 2"),
            Some(SlashCommand::Copy(Some("2".to_string())))
        );
    }

    #[test]
    fn slash_menu_exposes_the_public_session_lifecycle_commands() {
        let command_names = all_commands()
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();

        for command in ["/fork", "/rename", "/status", "/copy"] {
            assert!(command_names.contains(&command), "missing {command}");
        }
    }

    #[test]
    fn parses_goal_commands() {
        assert_eq!(
            parse("/goal"),
            Some(SlashCommand::Goal(GoalSlashCommand::Show))
        );
        assert_eq!(
            parse("/goal ship it"),
            Some(SlashCommand::Goal(GoalSlashCommand::Set(
                "ship it".to_string()
            )))
        );
        assert_eq!(
            parse("/goal edit better goal"),
            Some(SlashCommand::Goal(GoalSlashCommand::Edit(
                "better goal".to_string()
            )))
        );
        assert_eq!(
            parse("/goal clear"),
            Some(SlashCommand::Goal(GoalSlashCommand::Clear))
        );
        assert_eq!(
            parse("/goal pause"),
            Some(SlashCommand::Goal(GoalSlashCommand::Pause))
        );
        assert_eq!(
            parse("/goal resume"),
            Some(SlashCommand::Goal(GoalSlashCommand::Resume))
        );
        assert_eq!(parse("/goal edit"), None);
    }

    #[test]
    fn parses_queue_commands() {
        assert_eq!(
            parse("/queue"),
            Some(SlashCommand::Queue(QueueSlashCommand::List))
        );
        assert_eq!(
            parse("/queue list"),
            Some(SlashCommand::Queue(QueueSlashCommand::List))
        );
        assert_eq!(
            parse("/queue pause"),
            Some(SlashCommand::Queue(QueueSlashCommand::Pause))
        );
        assert_eq!(
            parse("/queue start"),
            Some(SlashCommand::Queue(QueueSlashCommand::Start))
        );
        assert_eq!(parse("/queue invalid"), None);
    }

    #[test]
    fn parses_trust_commands() {
        assert_eq!(
            parse("/trust"),
            Some(SlashCommand::Trust(TrustSlashCommand::Show))
        );
        assert_eq!(
            parse("/trust show"),
            Some(SlashCommand::Trust(TrustSlashCommand::Show))
        );
        assert_eq!(
            parse("/trust add"),
            Some(SlashCommand::Trust(TrustSlashCommand::Add))
        );
        assert_eq!(
            parse("/trust remove"),
            Some(SlashCommand::Trust(TrustSlashCommand::Remove))
        );
        assert_eq!(parse("/trust unknown"), None);
    }

    #[test]
    fn parses_workflows_command() {
        assert_eq!(parse("/workflows"), Some(SlashCommand::WorkflowList));
    }

    #[test]
    fn parses_saved_workflow_command() {
        assert_eq!(
            parse("/workflow:security-audit target=src maxAgents=8"),
            Some(SlashCommand::WorkflowRun {
                name: "security-audit".to_string(),
                args: Some("target=src maxAgents=8".to_string()),
            })
        );
        assert_eq!(parse("/workflow:"), None);

        let command_names = all_commands()
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();
        assert!(command_names.contains(&"/workflow:<name>"));
    }

    #[test]
    fn available_commands_include_project_saved_workflows() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_dir = temp.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        std::fs::write(
            workflow_dir.join("security-audit.js"),
            "export const meta = {};",
        )
        .unwrap();

        let command_names = available_commands(temp.path(), &[])
            .into_iter()
            .map(|(command, _)| command)
            .collect::<Vec<_>>();
        assert!(command_names.contains(&"/workflow:<name>".to_string()));
        assert!(command_names.contains(&"/workflow:security-audit".to_string()));
    }

    #[test]
    fn saved_workflow_aliases_are_available_only_without_builtin_collision() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_dir = temp.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        std::fs::write(
            workflow_dir.join("security-audit.js"),
            "export default 'ok';",
        )
        .unwrap();
        std::fs::write(workflow_dir.join("model.js"), "export default 'ok';").unwrap();

        let command_names = available_commands(temp.path(), &[])
            .into_iter()
            .map(|(command, _)| command)
            .collect::<Vec<_>>();

        assert!(command_names.contains(&"/security-audit".to_string()));
        assert!(command_names.contains(&"/workflow:security-audit".to_string()));
        assert!(command_names.contains(&"/workflow:model".to_string()));
        assert_eq!(
            command_names
                .iter()
                .filter(|command| command.as_str() == "/model")
                .count(),
            1
        );
    }

    #[test]
    fn parse_with_cwd_accepts_saved_workflow_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let workflow_dir = temp.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        std::fs::write(
            workflow_dir.join("security-audit.js"),
            "export default 'ok';",
        )
        .unwrap();
        std::fs::write(workflow_dir.join("model.js"), "export default 'ok';").unwrap();

        assert_eq!(
            parse_with_cwd("/security-audit target=src", temp.path(), &[]),
            Some(SlashCommand::WorkflowRun {
                name: "security-audit".to_string(),
                args: Some("target=src".to_string()),
            })
        );
        assert_eq!(
            parse_with_cwd("/model", temp.path(), &[]),
            Some(SlashCommand::Model(None))
        );
    }

    #[test]
    fn parses_agents_command() {
        assert_eq!(parse("/agents"), Some(SlashCommand::AgentDashboard));
    }

    #[test]
    fn parses_tasks_command() {
        assert_eq!(parse("/tasks"), Some(SlashCommand::TaskWorkspace));
        assert!(
            all_commands()
                .iter()
                .any(|(command, _)| *command == "/tasks")
        );
    }

    #[test]
    fn parses_child_follow_up_with_non_empty_prompt_only() {
        assert_eq!(
            parse("/task-follow-up child-1 inspect the failing test"),
            Some(SlashCommand::TaskFollowUp {
                task_id: "child-1".to_string(),
                prompt: "inspect the failing test".to_string(),
            })
        );
        assert_eq!(parse("/task-follow-up child-1   "), None);
        assert_eq!(parse("/task-follow-up   inspect"), None);
    }

    #[test]
    fn parses_config_command() {
        assert_eq!(parse("/config"), Some(SlashCommand::Config));
        assert_eq!(parse("/config show"), None);
    }

    #[test]
    fn parses_mcp_command() {
        assert_eq!(parse("/mcp"), Some(SlashCommand::Mcp));
        assert!(all_commands().contains(&("/mcp", "Manage MCP servers")));
    }

    fn mcp_prompt(server: &str, name: &str, arguments: &[(&str, bool)]) -> McpPromptView {
        McpPromptView {
            server: server.to_string(),
            name: name.to_string(),
            description: None,
            arguments: arguments
                .iter()
                .map(|(name, required)| ((*name).to_string(), *required))
                .collect(),
        }
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn positional_arguments_map_with_the_last_taking_the_rest() {
        let prompt = mcp_prompt("docs", "search", &[("a", true), ("b", true)]);

        assert_eq!(
            map_prompt_arguments(&prompt, "1 two words"),
            Ok(pairs(&[("a", "1"), ("b", "two words")]))
        );
        // Any run of whitespace ends a word; the last argument keeps the
        // rest as typed, line breaks included.
        assert_eq!(
            map_prompt_arguments(&prompt, "  1 \t two  words\nand more  "),
            Ok(pairs(&[("a", "1"), ("b", "two  words\nand more")]))
        );
        // An optional argument left out is not sent at all, not even as "".
        let review = mcp_prompt("github", "review_pr", &[("pr", true), ("branch", false)]);
        assert_eq!(
            map_prompt_arguments(&review, "123"),
            Ok(pairs(&[("pr", "123")]))
        );
        assert_eq!(
            map_prompt_arguments(&review, "123 main"),
            Ok(pairs(&[("pr", "123"), ("branch", "main")]))
        );
        assert_eq!(
            map_prompt_arguments(&mcp_prompt("docs", "status", &[]), "  "),
            Ok(Vec::new())
        );
    }

    #[test]
    fn an_unknown_mcp_command_is_not_parsed_as_a_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let prompts = [
            mcp_prompt("github", "review_pr", &[("pr", true)]),
            mcp_prompt("github", "two words", &[]),
        ];
        let parse = |input: &str| parse_with_cwd(input, temp.path(), &prompts);

        assert_eq!(
            parse("/mcp__github__review_pr 12  and more"),
            Some(SlashCommand::McpPrompt {
                server: "github".to_string(),
                prompt: "review_pr".to_string(),
                args: "12  and more".to_string(),
            })
        );
        for input in [
            "/mcp__github__review",
            "/mcp__github__review_pr_all",
            "/mcp__gitlab__review_pr 12",
            // The server is named as the catalog names it, as in tool names.
            "/mcp__GitHub__review_pr 12",
            // A prompt whose name holds whitespace cannot be typed.
            "/mcp__github__two words",
            "/mcp__github__two",
            "/mcp__github__",
            "/mcp__",
        ] {
            assert_eq!(parse(input), None, "{input}");
        }
        assert_eq!(
            invalid_slash_command_message("/mcp__github__review 12"),
            "unknown slash command `/mcp__github__review`. Type / to view available commands."
        );
        // `/mcp` still opens the panel, and takes no arguments.
        assert_eq!(parse("/mcp"), Some(SlashCommand::Mcp));
        assert_eq!(parse("/mcp review_pr"), None);
    }

    #[test]
    fn parses_remember_command() {
        assert_eq!(
            parse("/remember prefers rust"),
            Some(SlashCommand::Remember("prefers rust".to_string()))
        );
    }

    #[test]
    fn removed_terminal_aliases_are_not_slash_commands() {
        assert_eq!(parse("/help"), None);
        assert_eq!(parse("/exit"), None);

        let command_names = all_commands()
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();
        assert!(!command_names.contains(&"/help"));
        assert!(!command_names.contains(&"/clear"));
        assert!(!command_names.contains(&"/exit"));
    }

    #[test]
    fn exposes_presets_and_accepts_custom_models() {
        assert_eq!(
            available_models(),
            &["auto", "deepseek-flash", "deepseek-v4-pro"]
        );
        assert!(validate_model("auto").is_ok());
        assert!(validate_model("deepseek-flash").is_ok());
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-flash-vision-exp").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
        assert!(validate_model("deepseek-v4.1-flash-expires-on-0910").is_ok());
        assert!(validate_model("vendor/private-model:2026-09").is_ok());
        assert!(validate_model("").is_err());
        assert!(validate_model(" model").is_err());
    }

    #[test]
    fn parse_side_conversation_with_optional_question() {
        assert_eq!(parse("/side"), Some(SlashCommand::Side(None)));
        assert_eq!(
            parse("/side compare the two approaches"),
            Some(SlashCommand::Side(Some(
                "compare the two approaches".to_string()
            )))
        );
    }
}
use std::path::Path;

use crate::surface_projection::McpPromptView;
