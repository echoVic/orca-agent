#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    New,
    Model(Option<String>),
    Compact,
    Resume,
    Fork(Option<String>),
    Side(Option<String>),
    Rename(Option<String>),
    Status,
    Copy(Option<String>),
    CancelOperation,
    Cost,
    Config,
    Mode(Option<String>),
    Plan(Option<String>),
    Goal(GoalSlashCommand),
    Queue(QueueSlashCommand),
    WorkflowList,
    WorkflowRun { name: String, args: Option<String> },
    AgentDashboard,
    TaskWorkspace,
    TaskFollowUp { task_id: String, prompt: String },
    Remember(String),
    SkillList,
    SkillRun { id: String, args: Option<String> },
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

pub fn parse_with_cwd(input: &str, cwd: &Path) -> Option<SlashCommand> {
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

pub fn invalid_slash_command_message(input: &str) -> String {
    let command = input.trim().split_whitespace().next().unwrap_or("/");
    if command == "/workflow" {
        "invalid workflow command. Use /workflow:<name> [args].".to_string()
    } else {
        format!("unknown slash command `{command}`. Type / to view available commands.")
    }
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
        "resume" => no_arguments(parts).then_some(SlashCommand::Resume),
        "fork" => Some(SlashCommand::Fork(optional_argument(parts))),
        "side" => Some(SlashCommand::Side(optional_argument(parts))),
        "rename" => Some(SlashCommand::Rename(optional_argument(parts))),
        "status" => no_arguments(parts).then_some(SlashCommand::Status),
        "copy" => Some(SlashCommand::Copy(optional_argument(parts))),
        "cancel-operation" => no_arguments(parts).then_some(SlashCommand::CancelOperation),
        "cost" => no_arguments(parts).then_some(SlashCommand::Cost),
        "config" => no_arguments(parts).then_some(SlashCommand::Config),
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
        ("/resume", "Resume a saved conversation"),
        ("/fork", "Fork this conversation"),
        ("/side", "Ask without disrupting the main conversation"),
        ("/rename", "Rename this conversation"),
        ("/status", "Show session status"),
        ("/copy", "Copy an assistant response"),
        ("/cancel-operation", "Cancel a recoverable operation"),
        ("/cost", "Show session cost"),
        ("/config", "Configure runtime settings"),
        ("/mode", "Switch approval mode"),
        ("/plan", "Plan first, then approve implementation"),
        ("/goal", "Manage a persistent goal"),
        ("/queue", "List, pause, or start queued prompts"),
        ("/workflow:<name>", "Run a saved workflow"),
        ("/workflows", "Show workflow tasks"),
        ("/agents", "Open Agent Workspace"),
        ("/tasks", "Open unified Tasks workspace"),
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

pub fn available_commands(cwd: &Path) -> Vec<(String, String)> {
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
    commands
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

pub fn available_models() -> &'static [&'static str] {
    orca_core::model::allowed_models()
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

        let command_names = available_commands(temp.path())
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

        let command_names = available_commands(temp.path())
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
            parse_with_cwd("/security-audit target=src", temp.path()),
            Some(SlashCommand::WorkflowRun {
                name: "security-audit".to_string(),
                args: Some("target=src".to_string()),
            })
        );
        assert_eq!(
            parse_with_cwd("/model", temp.path()),
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
    fn validates_supported_models() {
        assert_eq!(
            available_models(),
            &[
                "auto",
                "deepseek-v4-flash",
                "deepseek-v4-flash-vision-exp",
                "deepseek-v4-pro"
            ]
        );
        assert!(validate_model("auto").is_ok());
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-flash-vision-exp").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
        assert!(validate_model("deepseek-reasoner").is_err());
        assert!(validate_model("bogus-model").is_err());
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
