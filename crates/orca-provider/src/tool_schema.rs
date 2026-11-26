use serde_json::{Value, json};

pub const DEEPSEEK_MAX_TOOLS: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict_capable: bool,
}

/// Keeps non-MCP tools first, then MCP tools by name, preserving survivor order.
/// When the caller has no tool source, names beginning with `mcp__` count as MCP.
pub fn limit_tool_definitions(
    definitions: &mut Vec<ProviderToolDefinition>,
    max: usize,
) -> Vec<String> {
    limit_tool_definitions_by_source(definitions, max, |_| None)
}

pub fn limit_tool_definitions_by_source(
    definitions: &mut Vec<ProviderToolDefinition>,
    max: usize,
    is_mcp: impl Fn(&str) -> Option<bool>,
) -> Vec<String> {
    limit_tools(definitions, max, |tool| &tool.name, is_mcp)
}

pub(crate) fn limit_tools_by_name<T>(
    tools: &mut Vec<T>,
    max: usize,
    name: impl Fn(&T) -> &str,
) -> Vec<String> {
    limit_tools(tools, max, name, |_| None)
}

fn limit_tools<T>(
    tools: &mut Vec<T>,
    max: usize,
    name: impl Fn(&T) -> &str,
    is_mcp: impl Fn(&str) -> Option<bool>,
) -> Vec<String> {
    if tools.len() <= max {
        return Vec::new();
    }
    let mut ranked = tools.iter().map(&name).enumerate().collect::<Vec<_>>();
    ranked.sort_by_cached_key(|&(_, name)| {
        (
            is_mcp(name).unwrap_or_else(|| name.starts_with("mcp__")),
            name,
        )
    });
    let mut keep = vec![true; tools.len()];
    let mut removed = Vec::with_capacity(tools.len() - max);
    for (index, name) in ranked.into_iter().skip(max) {
        keep[index] = false;
        removed.push(name.to_owned());
    }
    let mut keep = keep.into_iter();
    tools.retain(|_| keep.next().expect("one decision per tool"));
    removed.sort_unstable();
    removed
}

pub fn deepseek_tools_schema(definitions: &[ProviderToolDefinition]) -> Vec<Value> {
    let mut tools = definitions
        .iter()
        .map(deepseek_tool_schema)
        .collect::<Vec<_>>();
    sort_tools_by_name(&mut tools);
    tools
}

pub fn deepseek_strict_tools_schema_for_endpoint(
    definitions: &[ProviderToolDefinition],
    base_url: &str,
) -> Option<Vec<Value>> {
    if !is_strict_capable_endpoint(base_url)
        || !definitions
            .iter()
            .any(|definition| definition.strict_capable)
    {
        return None;
    }

    let mut tools = definitions
        .iter()
        .map(|definition| {
            let mut tool = deepseek_tool_schema(definition);
            if definition.strict_capable {
                let function = tool["function"]
                    .as_object_mut()
                    .expect("provider-generated function object");
                require_all_properties(
                    function
                        .get_mut("parameters")
                        .expect("provider-generated parameters"),
                );
                function.insert("strict".to_string(), Value::Bool(true));
                tool.sort_all_objects();
            }
            tool
        })
        .collect::<Vec<_>>();
    sort_tools_by_name(&mut tools);
    Some(tools)
}

fn sort_tools_by_name(tools: &mut [Value]) {
    tools.sort_by(|left, right| {
        left["function"]["name"]
            .as_str()
            .cmp(&right["function"]["name"].as_str())
    });
}

/// DeepSeek has always received tool schemas with sorted keys, and a prompt
/// cache prefix depends on the exact bytes. serde_json keeps insertion order
/// once a dependency turns on `preserve_order` (the ACP SDK does), so sort.
fn deepseek_tool_schema(definition: &ProviderToolDefinition) -> Value {
    let mut tool = json!({
        "type": "function",
        "function": {
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.input_schema,
        }
    });
    tool.sort_all_objects();
    tool
}

/// Whether `base_url` is an endpoint that accepts the strict tools payload.
///
/// Only DeepSeek's official beta endpoint is known to support it. Compatible
/// third-party endpoints rarely implement strict mode, so they stay off unless
/// the user explicitly forces it with `ORCA_STRICT_TOOLS=1` after checking
/// that their endpoint accepts `strict: true` function schemas.
fn is_strict_capable_endpoint(base_url: &str) -> bool {
    let url = base_url.trim_end_matches('/');
    url.ends_with("/beta") || std::env::var("ORCA_STRICT_TOOLS").as_deref() == Ok("1")
}

/// Serializes tests that read or write `ORCA_STRICT_TOOLS`.
#[cfg(test)]
pub(crate) static STRICT_TOOLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn require_all_properties(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let is_typed_object = match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("object")),
        _ => false,
    };
    if is_typed_object {
        object.insert("additionalProperties".to_string(), Value::Bool(false));
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            let required = properties.keys().cloned().map(Value::String).collect();
            object.insert("required".to_string(), Value::Array(required));
        }
    }

    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            require_all_properties(property);
        }
    }
    if let Some(items) = object.get_mut("items") {
        require_all_properties(items);
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for branch in branches {
                require_all_properties(branch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(strict_capable: bool) -> ProviderToolDefinition {
        ProviderToolDefinition {
            name: "demo".to_string(),
            description: "demo tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "required_value": { "type": "string" },
                    "optional_value": { "type": ["string", "null"] },
                    "nested": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    },
                    "nullable_nested": {
                        "type": ["object", "null"],
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                },
                "required": ["required_value"],
                "additionalProperties": false
            }),
            strict_capable,
        }
    }

    fn named_definition(name: &str, strict_capable: bool) -> ProviderToolDefinition {
        ProviderToolDefinition {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }),
            strict_capable,
        }
    }

    #[test]
    fn limit_keeps_every_builtin_tool_and_drops_mcp_tools_past_the_limit() {
        let builtins = (0..30).map(|i| format!("b{i:02}")).collect::<Vec<_>>();
        let mcp = (0..110)
            .map(|i| format!("mcp__srv__t{i:03}"))
            .collect::<Vec<_>>();
        let mut defs = mcp
            .iter()
            .chain(&builtins)
            .rev()
            .map(|name| named_definition(name, false))
            .collect::<Vec<_>>();
        let original = defs.clone();

        assert_eq!(limit_tool_definitions(&mut defs, 128), mcp[98..]);
        assert_eq!(
            defs,
            original
                .into_iter()
                .filter(|tool| { builtins.contains(&tool.name) || mcp[..98].contains(&tool.name) })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn limit_leaves_a_list_under_the_limit_unchanged() {
        let mut defs = ["z", "mcp__srv__z", "a", "mcp__srv__a", "b"]
            .map(|name| named_definition(name, false))
            .to_vec();
        let original = defs.clone();

        assert!(limit_tool_definitions(&mut defs, 128).is_empty());
        assert_eq!(defs, original);
    }

    #[test]
    fn limit_truncates_builtins_only_when_they_alone_exceed_the_limit() {
        let builtins = (0..130).map(|i| format!("b{i:03}")).collect::<Vec<_>>();
        let mcp = (0..3).map(|i| format!("mcp__srv__t{i:03}"));
        let mut defs = mcp
            .chain(builtins.iter().rev().cloned())
            .map(|name| named_definition(&name, false))
            .collect::<Vec<_>>();

        assert_eq!(
            limit_tool_definitions(&mut defs, 128),
            [
                "b128",
                "b129",
                "mcp__srv__t000",
                "mcp__srv__t001",
                "mcp__srv__t002"
            ]
        );
        assert_eq!(
            defs.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
            builtins[..128].iter().rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn base_lowering_is_deterministic_and_omits_strict() {
        let definitions = vec![definition(true)];
        let first = deepseek_tools_schema(&definitions);
        let second = deepseek_tools_schema(&definitions);

        assert_eq!(first, second);
        assert!(first[0]["function"].get("strict").is_none());
    }

    #[test]
    fn strict_lowering_uses_definition_metadata_instead_of_tool_names() {
        let definitions = vec![definition(true)];
        let tools = deepseek_strict_tools_schema_for_endpoint(
            &definitions,
            "https://api.deepseek.com/beta",
        )
        .expect("strict tools");

        assert_eq!(tools[0]["function"]["strict"], true);
        assert_eq!(
            tools[0]["function"]["parameters"]["required"],
            json!([
                "nested",
                "nullable_nested",
                "optional_value",
                "required_value"
            ])
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["nested"]["additionalProperties"],
            false
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["nullable_nested"]["additionalProperties"],
            false
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["nullable_nested"]["required"],
            json!(["name"])
        );
    }

    #[test]
    fn tool_definition_order_does_not_change_plain_payload() {
        let alpha = named_definition("alpha", false);
        let zeta = named_definition("zeta", false);

        assert_eq!(
            deepseek_tools_schema(&[alpha.clone(), zeta.clone()]),
            deepseek_tools_schema(&[zeta, alpha])
        );
    }

    #[test]
    fn tool_definition_order_does_not_change_strict_payload() {
        let alpha = named_definition("alpha", true);
        let zeta = named_definition("zeta", true);

        assert_eq!(
            deepseek_strict_tools_schema_for_endpoint(
                &[alpha.clone(), zeta.clone()],
                "https://api.deepseek.com/beta",
            ),
            deepseek_strict_tools_schema_for_endpoint(
                &[zeta, alpha],
                "https://api.deepseek.com/beta",
            )
        );
    }

    fn with_strict_tools_env<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        let _guard = STRICT_TOOLS_ENV_LOCK.lock().unwrap();
        let previous = std::env::var("ORCA_STRICT_TOOLS").ok();
        unsafe {
            match value {
                Some(value) => std::env::set_var("ORCA_STRICT_TOOLS", value),
                None => std::env::remove_var("ORCA_STRICT_TOOLS"),
            }
        }
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("ORCA_STRICT_TOOLS", value),
                        None => std::env::remove_var("ORCA_STRICT_TOOLS"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        test()
    }

    #[test]
    fn custom_and_trailing_slash_endpoints_are_not_strict_by_default() {
        with_strict_tools_env(None, || {
            for base_url in [
                "https://api.siliconflow.cn/v1",
                "https://api.siliconflow.cn/v1/",
                "https://openrouter.ai/api/v1",
                "http://localhost:11434/v1",
                "https://api.deepseek.com",
            ] {
                assert!(
                    !is_strict_capable_endpoint(base_url),
                    "{base_url} must not get the strict payload by default"
                );
            }
            assert!(is_strict_capable_endpoint("https://api.deepseek.com/beta"));
            assert!(is_strict_capable_endpoint("https://api.deepseek.com/beta/"));
        });
    }

    #[test]
    fn strict_tools_can_be_forced_for_a_custom_endpoint() {
        with_strict_tools_env(Some("1"), || {
            assert!(is_strict_capable_endpoint("https://api.siliconflow.cn/v1"));
            assert!(is_strict_capable_endpoint("http://localhost:11434/v1/"));
        });
        with_strict_tools_env(Some("true"), || {
            assert!(
                !is_strict_capable_endpoint("https://api.siliconflow.cn/v1"),
                "only the exact value 1 enables strict tools"
            );
        });
    }
}
