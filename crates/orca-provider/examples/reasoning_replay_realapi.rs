//! Real-API probe: does DeepSeek V4 thinking mode require `reasoning_content`
//! to be replayed on assistant tool-call messages when continuing a tool loop?
//!
//! DeepSeek's thinking-mode docs state: "When the model performs a tool call,
//! the reasoning_content must be fully passed back to the API in all
//! subsequent requests. If your code does not correctly pass back
//! reasoning_content, the API will return a 400 error."
//!
//! This harness measures what the live API does for both the required replay
//! style and an intentionally invalid stripped-reasoning control, on the
//! default and beta endpoints. Makes REAL, BILLED calls.
//!
//! Run: `cargo run -p orca-provider --example reasoning_replay_realapi`

use std::collections::HashMap;

use orca_provider::tool_schema::{ProviderToolDefinition, deepseek_tools_schema};
use serde_json::{Value, json};

const MODEL: &str = "deepseek-flash";
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

fn update_plan_tool_schema() -> Vec<Value> {
    let definitions = vec![ProviderToolDefinition {
        name: "update_plan".to_string(),
        description: "Update the current execution plan.".to_string(),
        input_schema: update_plan_input_schema(),
        strict_capable: true,
    }];
    deepseek_tools_schema(&definitions)
}

fn update_plan_input_schema() -> Value {
    json!({
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

fn post(api_key: &str, base_url: &str, body: &Value) -> Result<Value, String> {
    let url = format!("{base_url}/chat/completions");
    let response = orca_provider::http_client::execute_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(body)
    })?;
    response
        .json::<Value>()
        .map_err(|error| format!("response not JSON: {error}"))
}

struct FirstTurn {
    assistant_content: Option<String>,
    reasoning: Option<String>,
    tool_call: Value,
    tool_call_id: String,
}

fn first_turn(api_key: &str, base_url: &str) -> Result<FirstTurn, String> {
    let body = json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": USER_PROMPT},
        ],
        "tools": update_plan_tool_schema(),
        "thinking": {"type": "enabled"},
        "reasoning_effort": "max",
        "max_tokens": 384_000,
        "stream": false,
    });
    let payload = post(api_key, base_url, &body)?;
    let message = payload
        .pointer("/choices/0/message")
        .cloned()
        .ok_or("no message in first response")?;
    let tool_call = message
        .pointer("/tool_calls/0")
        .cloned()
        .ok_or_else(|| format!("no tool call in first response: {message}"))?;
    let tool_call_id = tool_call["id"]
        .as_str()
        .ok_or("tool call missing id")?
        .to_string();
    Ok(FirstTurn {
        assistant_content: message["content"].as_str().map(str::to_string),
        reasoning: message["reasoning_content"].as_str().map(str::to_string),
        tool_call,
        tool_call_id,
    })
}

/// Continue the tool loop, either replaying reasoning_content or intentionally
/// stripping it as a negative control. Returns (ok, detail).
fn continue_loop(
    api_key: &str,
    base_url: &str,
    turn: &FirstTurn,
    with_reasoning: bool,
) -> (bool, String) {
    let mut assistant = json!({
        "role": "assistant",
        "content": turn.assistant_content,
        "tool_calls": [turn.tool_call],
    });
    if with_reasoning && let Some(reasoning) = &turn.reasoning {
        assistant["reasoning_content"] = Value::String(reasoning.clone());
    }
    let body = json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": USER_PROMPT},
            assistant,
            {
                "role": "tool",
                "tool_call_id": turn.tool_call_id,
                "content": "Plan updated (3 item(s)).\n  [>] Design the endpoint contract\n  [ ] Implement the handler\n  [ ] Write integration tests",
            },
        ],
        "tools": update_plan_tool_schema(),
        "thinking": {"type": "enabled"},
        "reasoning_effort": "max",
        "max_tokens": 384_000,
        "stream": false,
    });
    match post(api_key, base_url, &body) {
        Ok(payload) => {
            let content = payload
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let usage_prompt = payload
                .pointer("/usage/prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let usage_reasoning = payload
                .pointer("/usage/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (
                true,
                format!(
                    "ok prompt_tokens={usage_prompt} reasoning_tokens={usage_reasoning} content_preview={:?}",
                    content.chars().take(80).collect::<String>()
                ),
            )
        }
        Err(error) => (false, format!("REJECTED: {error}")),
    }
}

fn main() {
    let Some(api_key) = load_api_key() else {
        eprintln!("DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping.");
        std::process::exit(1);
    };

    println!("== Real-API reasoning_content replay probe (model = {MODEL}) ==\n");

    for base_url in ["https://api.deepseek.com", "https://api.deepseek.com/beta"] {
        println!("-- endpoint: {base_url} --");
        let turn = match first_turn(&api_key, base_url) {
            Ok(turn) => turn,
            Err(error) => {
                println!("  first turn failed: {error}\n");
                continue;
            }
        };
        println!(
            "  first turn: reasoning_content={} chars, tool_call={}",
            turn.reasoning.as_deref().map(str::len).unwrap_or(0),
            turn.tool_call["function"]["name"]
        );

        let (with_ok, with_detail) = continue_loop(&api_key, base_url, &turn, true);
        let (without_ok, without_detail) = continue_loop(&api_key, base_url, &turn, false);
        println!("  replay WITH reasoning_content    -> {with_detail}");
        println!("  replay WITHOUT reasoning_content -> {without_detail}");
        let verdict = match (with_ok, without_ok) {
            (true, false) => "required replay accepted; negative control rejected",
            (true, true) => "both accepted (negative control tolerated today)",
            (false, _) => "docs-style replay itself failed — inspect detail",
        };
        println!("  verdict: {verdict}\n");
    }
}
