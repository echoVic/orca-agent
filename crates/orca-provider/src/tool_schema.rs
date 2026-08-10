use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict_capable: bool,
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

fn deepseek_tool_schema(definition: &ProviderToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.input_schema,
        }
    })
}

fn is_strict_capable_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/').ends_with("/beta")
}

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
}
