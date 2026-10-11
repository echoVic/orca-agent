use orca_core::approval_rules::CompiledPermissionRules;
use orca_core::approval_types::Decision;
use orca_core::config::RunConfig;

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
}
