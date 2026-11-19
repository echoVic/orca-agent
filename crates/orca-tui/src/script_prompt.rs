//! What `/scripts` prints: whether `run_script` is offered, its limits, and
//! the description and parameters the model is sent for it. The last two are
//! read from the tool registry, so they cannot drift from what is sent.

use orca_core::config::RunConfig;

pub(crate) fn format_scripts(config: &RunConfig) -> String {
    let script = &config.tools.script;
    let mut lines = Vec::new();
    if script.enabled {
        lines.push("run_script is on".to_string());
    } else {
        lines.push("run_script is off".to_string());
        lines.push(
            "  set `enabled = true` under [tools.script] in the user configuration and start a new session"
                .to_string(),
        );
    }
    lines.push(format!(
        "  {}s per script · {} tool calls · {} MiB · {} calls at once · {} KiB per result",
        script.timeout_secs,
        script.max_tool_calls,
        script.memory_limit_bytes() / (1024 * 1024),
        config.tools.max_read_parallel,
        orca_runtime::script::script_result_limit_bytes() / 1024,
    ));
    lines.push(format!(
        "  a script can call: {}",
        orca_runtime::script::script_tools().join(", ")
    ));

    let registry = orca_tools::registry::default_tool_registry();
    if let Some(resolved) = registry.resolve(crate::script_run::SCRIPT_TOOL) {
        lines.push(String::new());
        lines.push("Description sent to the model".to_string());
        lines.push(format!("  {}", resolved.spec.description));
        lines.push(String::new());
        lines.push("Parameters sent to the model".to_string());
        let schema = serde_json::to_string_pretty(&resolved.spec.input_schema)
            .unwrap_or_else(|_| resolved.spec.input_schema.to_string());
        for line in schema.lines() {
            lines.push(format!("  {line}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_run_config;

    fn registered() -> (String, String) {
        let registry = orca_tools::registry::default_tool_registry();
        let resolved = registry.resolve("run_script").expect("run_script tool");
        let schema = serde_json::to_string_pretty(&resolved.spec.input_schema).unwrap();
        (resolved.spec.description.clone(), schema)
    }

    fn indented(text: &str) -> String {
        text.lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn on_it_says_on_its_limits_and_what_the_model_is_sent() {
        let mut config = test_run_config();
        config.tools.script.enabled = true;
        let text = format_scripts(&config);
        let (description, schema) = registered();

        assert_eq!(text.lines().next(), Some("run_script is on"));
        assert!(text.contains(
            "60s per script · 100 tool calls · 64 MiB · 8 calls at once · 256 KiB per result"
        ));
        assert!(text.contains("a script can call: read_file, glob, grep, git_status"));
        assert!(text.contains(&format!("Description sent to the model\n  {description}\n")));
        assert!(text.contains(&format!(
            "Parameters sent to the model\n{}",
            indented(&schema)
        )));
    }

    #[test]
    fn off_it_says_how_to_turn_it_on_and_still_shows_what_would_be_sent() {
        let mut config = test_run_config();
        config.tools.script.enabled = false;
        let text = format_scripts(&config);
        let (description, schema) = registered();

        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("run_script is off"));
        assert!(lines.next().unwrap().contains(
            "set `enabled = true` under [tools.script] in the user configuration and start a new session"
        ));
        assert!(text.contains(&description));
        assert!(text.contains(&indented(&schema)));
    }

    #[test]
    fn the_limits_are_the_sessions_own() {
        let mut config = test_run_config();
        config.tools.script.timeout_secs = 5;
        config.tools.script.max_tool_calls = 7;
        config.tools.script.memory_limit_mb = 16;
        config.tools.max_read_parallel = 2;
        assert!(format_scripts(&config).contains(
            "5s per script · 7 tool calls · 16 MiB · 2 calls at once · 256 KiB per result"
        ));

        config.tools.script.memory_limit_mb = 0;
        assert!(format_scripts(&config).contains("· 8 MiB ·"));
    }
}
