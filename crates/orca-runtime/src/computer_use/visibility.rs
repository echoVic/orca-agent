use orca_core::approval_rules::CompiledPermissionRules;
use orca_core::approval_types::Decision;
use orca_core::config::RunConfig;

pub(crate) fn session_visible(config: &RunConfig, subagent_depth: u32) -> bool {
    tool_visible(
        config,
        subagent_depth,
        configured_tools(config),
        orca_computer_use::apps::host_supported(),
    )
}

pub(crate) fn configured_tools(config: &RunConfig) -> Option<&[String]> {
    config
        .subagents
        .effective_definition
        .as_ref()
        .map(|definition| definition.allowed_tools.as_slice())
        .or(config.subagents.inherited_tools.as_deref())
}

pub(crate) fn tool_visible(
    config: &RunConfig,
    subagent_depth: u32,
    allowed_tools: Option<&[String]>,
    host_supported: bool,
) -> bool {
    host_supported
        && config.computer_use.interactive
        && subagent_depth == 0
        && allowed_tools.is_none()
        && CompiledPermissionRules::from_rules(config.permission_rules.clone())
            .matching_decision("computer", None)
            != Some(Decision::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRule;
    use orca_core::approval_types::Decision;

    #[test]
    fn the_tool_shows_only_in_the_tuis_main_session_on_a_supported_mac() {
        let mut config = RunConfig::default();
        config.computer_use.interactive = true;
        assert!(tool_visible(&config, 0, None, true));
        assert!(!tool_visible(&config, 0, None, false));
        assert!(!tool_visible(&config, 1, None, true));
        assert!(!tool_visible(
            &config,
            0,
            Some(&["read_file".to_string()]),
            true
        ));
        config.permission_rules.rules.push(PermissionRule::new(
            "computer",
            "com.apple.Safari",
            Decision::Deny,
        ));
        assert!(tool_visible(&config, 0, None, true));
        config
            .permission_rules
            .rules
            .push(PermissionRule::whole_tool("computer", Decision::Deny));
        assert!(!tool_visible(&config, 0, None, true));
        assert!(!tool_visible(&RunConfig::default(), 0, None, true));
    }
    #[test]
    fn configured_tools_include_inherited_restrictions() {
        let mut config = RunConfig::default();
        config.computer_use.interactive = true;
        assert!(configured_tools(&config).is_none());
        assert!(tool_visible(&config, 0, configured_tools(&config), true));
        for tools in [
            vec![],
            vec!["read_file".to_string(), "computer".to_string()],
        ] {
            config.subagents.inherited_tools = Some(tools.clone());
            assert_eq!(configured_tools(&config), Some(tools.as_slice()));
            assert!(!tool_visible(&config, 0, configured_tools(&config), true));
        }
    }
}
