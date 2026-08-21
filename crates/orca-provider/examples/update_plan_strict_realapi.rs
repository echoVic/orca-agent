//! Real-API harness for the update_plan reliability work (normalization +
//! DeepSeek strict mode). Makes REAL, BILLED DeepSeek calls.
//!
//! Verifies against the live API:
//!   1. default endpoint: a plan-bait prompt yields an update_plan call whose
//!      arguments survive schema validation and execute cleanly end-to-end
//!   2. beta endpoint accepts our strict-transformed update_plan schema
//!      (server-side schema validation returns 200, not 400)
//!   3. strict-mode arguments parse as JSON (no DeepSeek-V3#1069 malformed
//!      output) and carry the strict shape (explanation always present, no
//!      unknown keys, valid statuses)
//!   4. beta endpoint end-to-end through the provider path (exercises the
//!      strict wiring and, if the server rejected strict, the fallback retry)
//!
//! Requires DEEPSEEK_API_KEY (env var or ~/.orca/auth.json).
//! Run: `cargo run -p orca-provider --example update_plan_strict_realapi`

use std::collections::HashMap;

use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::provider_types::ProviderStep;
use orca_core::tool_types::ToolName;
use orca_provider::tool_schema::{
    ProviderToolDefinition, deepseek_strict_tools_schema_for_endpoint,
};
use orca_provider::{ProviderConfig, call};
use serde_json::Value;

const MODEL: &str = "deepseek-v4-flash";
const DEFAULT_URL: &str = "https://api.deepseek.com";
const BETA_URL: &str = "https://api.deepseek.com/beta";
const SYSTEM_PROMPT: &str = "You are Orca, a terminal coding agent. When given a multi-step task you MUST record the plan by calling the update_plan tool before writing any prose.";
const USER_PROMPT: &str = "Task: add a /health endpoint to the API server. Record a 3-step plan now via update_plan: 'Design the endpoint contract' (in_progress), 'Implement the handler' (pending), 'Write integration tests' (pending). Call the tool, do not answer in prose.";

fn load_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let path = dirs::home_dir()?.join(".orca").join("auth.json");
    let content = std::fs::read_to_string(path).ok()?;
    let map: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    map.get("DEEPSEEK_API_KEY")
        .filter(|k| !k.is_empty())
        .cloned()
}

fn update_plan_tool_definitions() -> Vec<ProviderToolDefinition> {
    vec![ProviderToolDefinition {
        name: "update_plan".to_string(),
        description: "Update the current execution plan.".to_string(),
        input_schema: update_plan_input_schema(),
        strict_capable: true,
    }]
}

fn update_plan_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "explanation": { "type": ["string", "null"] },
            "plan": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "step": { "type": "string" },
                        "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                    },
                    "required": ["step", "status"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["plan"],
        "additionalProperties": false
    })
}

fn provider_config(api_key: &str, base_url: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        api_key: Some(api_key.to_string()),
        base_url: base_url.map(str::to_string),
        model: Some(MODEL.to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::default(),
        tools_override: Some(update_plan_tool_definitions()),
        mcp_registry: None,
        external_tools: Vec::new(),
    }
}

fn plan_bait_conversation() -> Conversation {
    let mut conv = Conversation::new();
    conv.add_system(SYSTEM_PROMPT.to_string());
    conv.add_user(USER_PROMPT.to_string());
    conv
}

/// Runs a provider-path call and checks that it yields a JSON update_plan
/// request. Retries once if the model answered in prose instead of calling the
/// tool.
fn provider_path_check(label: &str, config: &ProviderConfig) -> (bool, String) {
    for attempt in 0..2 {
        let response = call(ProviderKind::DeepSeek, &plan_bait_conversation(), config);
        let errors: Vec<&str> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::Error(error) => Some(error.message.as_str()),
                _ => None,
            })
            .collect();
        if !errors.is_empty() {
            return (false, format!("provider error: {}", errors.join("; ")));
        }
        let Some(request) = response.steps.iter().find_map(|step| match step {
            ProviderStep::ToolCall(request) if request.name == ToolName::UpdatePlan => {
                Some(request.clone())
            }
            _ => None,
        }) else {
            if attempt == 0 {
                eprintln!("  [{label}] no tool call on attempt 1, retrying once...");
                continue;
            }
            return (
                false,
                format!(
                    "model never called update_plan; content={:?}",
                    response.assistant_content.as_deref().unwrap_or_default()
                ),
            );
        };

        let raw = request.raw_arguments.clone().unwrap_or_default();
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                return (
                    false,
                    format!("tool arguments were not JSON: {error}; raw={raw}"),
                );
            }
        };
        if !parsed["plan"].is_array() {
            return (false, format!("tool arguments omit plan array; raw={raw}"));
        }
        return (true, format!("args={raw}"));
    }
    unreachable!("loop always returns")
}

/// Raw HTTP probe against the beta endpoint with strict tools: proves whether
/// the server accepts our strict schema and returns well-formed arguments.
fn strict_probe(api_key: &str) -> (bool, bool, String) {
    let tools =
        deepseek_strict_tools_schema_for_endpoint(&update_plan_tool_definitions(), BETA_URL)
            .expect("beta endpoint must qualify update_plan for strict mode");
    let body = serde_json::json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": USER_PROMPT},
        ],
        "tools": tools,
        "stream": false,
    });
    let url = format!("{BETA_URL}/chat/completions");
    let response = orca_provider::http_client::execute_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(&body)
    });

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            // A 400 here means the server rejected the strict schema itself.
            return (false, false, format!("strict request rejected: {error}"));
        }
    };
    let payload: Value = match response.json() {
        Ok(payload) => payload,
        Err(error) => return (true, false, format!("response not JSON: {error}")),
    };
    let Some(arguments) = payload
        .pointer("/choices/0/message/tool_calls/0/function/arguments")
        .and_then(Value::as_str)
    else {
        let content = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return (
            true,
            false,
            format!("no tool call in strict response; content={content:?}"),
        );
    };

    // #1069 regression check: strict arguments must be valid JSON.
    let parsed: Value = match serde_json::from_str(arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                true,
                false,
                format!(
                    "MALFORMED JSON from strict mode (#1069 still live): {error}; raw={arguments}"
                ),
            );
        }
    };

    // Strict shape: explanation always present (nullable), items carry only
    // step/status with valid enum values.
    let explanation_present = parsed.get("explanation").is_some();
    let items_ok = parsed["plan"].as_array().is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|item| {
                item.as_object().is_some_and(|object| {
                    object.len() == 2
                        && object.get("step").is_some_and(Value::is_string)
                        && object
                            .get("status")
                            .and_then(Value::as_str)
                            .is_some_and(|status| {
                                ["pending", "in_progress", "completed"].contains(&status)
                            })
                })
            })
    });
    let conforms = explanation_present && items_ok;
    let detail =
        format!("explanation_present={explanation_present} items_ok={items_ok} args={arguments}");
    (true, conforms, detail)
}

fn verdict(name: &str, pass: bool, detail: &str) {
    let tag = if pass { "PASS" } else { "FAIL" };
    println!("  [{tag}] {name}\n         {detail}");
}

fn main() {
    let Some(api_key) = load_api_key() else {
        eprintln!("DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping real-API eval.");
        std::process::exit(1);
    };

    println!("== Real-API update_plan strict/normalization checks ==");
    println!("(model = {MODEL})\n");

    let (default_ok, default_detail) =
        provider_path_check("default", &provider_config(&api_key, Some(DEFAULT_URL)));
    let (strict_accepted, strict_conforms, strict_detail) = strict_probe(&api_key);
    let (beta_ok, beta_detail) =
        provider_path_check("beta", &provider_config(&api_key, Some(BETA_URL)));

    println!("== Verdicts ==");
    verdict(
        "default endpoint: update_plan call validates in provider path",
        default_ok,
        &default_detail,
    );
    verdict(
        "beta endpoint accepts strict update_plan schema",
        strict_accepted,
        &strict_detail,
    );
    verdict(
        "strict arguments well-formed and schema-conforming",
        strict_conforms,
        &strict_detail,
    );
    verdict(
        "beta endpoint end-to-end via provider path",
        beta_ok,
        &beta_detail,
    );

    if default_ok && strict_accepted && strict_conforms && beta_ok {
        println!("\nALL REAL-API CHECKS PASSED");
    } else {
        println!("\nSOME REAL-API CHECKS FAILED");
        std::process::exit(2);
    }
}
