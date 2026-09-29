//! Real-API probe: does the history stay cached when only the internal
//! context (plan, goal, memory, mode) changes?
//!
//! Orca sends that context as the last message of the request, after the
//! history. This probe runs one tool-loop conversation twice with a changed
//! plan, first as Orca lowers it and then with the context placed ahead of
//! the history, the way requests were built before v0.5.4. It also confirms
//! the API accepts a system message after tool results. Makes REAL, BILLED
//! calls: five small requests.
//!
//! Run: `cargo run -p orca-provider --example internal_context_tail_realapi`

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_core::config::ProviderKind;
use orca_core::conversation::{Conversation, Message};
use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
use orca_provider::tool_schema::ProviderToolDefinition;
use orca_provider::{ProviderConfig, call};
use serde_json::json;

fn load_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let path = dirs::home_dir()?.join(".orca").join("auth.json");
    let content = std::fs::read_to_string(path).ok()?;
    let values: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    values
        .get("DEEPSEEK_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
}

fn read_file_tool() -> ProviderToolDefinition {
    ProviderToolDefinition {
        name: "read_file".to_string(),
        description: "Read a text file from the workspace.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
        strict_capable: false,
    }
}

/// A fresh prefix per run, so no earlier probe's cache inflates the hits.
fn instructions() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let segments = (0..48)
        .map(|index| {
            format!(
                "Instruction {index}: keep answers short and read files before describing them."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("Probe run {nonce}. You are a coding agent. {segments}")
}

fn notes() -> String {
    (0..200)
        .map(|index| {
            format!("Line {index}: module {index} keeps its build cache between incremental runs.")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn plan(version: u32) -> String {
    format!(
        "[Pinned plan state]\n[completed] Read notes.txt\n[in_progress] Summarize revision {version}\n[pending] Report back"
    )
}

fn send(conversation: &Conversation, config: &ProviderConfig, label: &str) -> ProviderResponse {
    let response = call(ProviderKind::DeepSeek, conversation, config);
    if let Some(ProviderStep::Error(error)) = response
        .steps
        .iter()
        .find(|step| matches!(step, ProviderStep::Error(_)))
    {
        eprintln!("{label}: DeepSeek rejected the request: {error:?}");
        std::process::exit(1);
    }
    let usage = response.usage.unwrap_or_default();
    println!(
        "{label}: prompt_tokens={} cache_tokens={}",
        usage.input_tokens, usage.cache_tokens
    );
    response
}

fn usage(response: &ProviderResponse) -> Usage {
    response.usage.unwrap_or_default()
}

fn main() {
    let Some(api_key) = load_api_key() else {
        eprintln!("DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping.");
        return;
    };
    let config = ProviderConfig {
        api_key: Some(api_key),
        base_url: None,
        model: Some("deepseek-flash".to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::default(),
        tools_override: Some(vec![read_file_tool()]),
        mcp_registry: None,
        external_tools: Vec::new(),
        max_output_tokens: None,
    };
    // Give the provider time to store a prefix before the request that reuses it.
    let settle = || std::thread::sleep(Duration::from_secs(3));

    // A real tool call, so its reasoning replays the way thinking mode requires.
    let mut history = Conversation::new();
    history.add_system(instructions());
    history.add_user(
        "Call read_file with path notes.txt, then describe it in one short sentence.".to_string(),
    );
    let first = send(&history, &config, "tool call");
    if first.tool_calls.is_empty() {
        eprintln!("the first response made no tool call; rerun the probe");
        std::process::exit(1);
    }
    history.messages.push(Message::Assistant {
        content: first.assistant_content.clone(),
        reasoning_content: first.assistant_reasoning.clone(),
        tool_calls: first.tool_calls.clone(),
        pinned: false,
    });
    for tool_call in &first.tool_calls {
        history.add_tool_result(tool_call.id.clone(), notes());
    }

    let mut last = history.clone();
    last.replace_plan_state(plan(1));
    send(&last, &config, "context last, plan 1");
    settle();
    last.replace_plan_state(plan(2));
    let last_changed = usage(&send(&last, &config, "context last, plan 2"));

    let ahead = |version| {
        let mut conversation = history.clone();
        conversation.messages.insert(
            1,
            Message::System {
                content: plan(version),
                pinned: false,
            },
        );
        conversation
    };
    send(&ahead(3), &config, "context ahead, plan 3");
    settle();
    let ahead_changed = usage(&send(&ahead(4), &config, "context ahead, plan 4"));

    let last_share = last_changed.cache_tokens as f64 / last_changed.input_tokens.max(1) as f64;
    let ahead_share = ahead_changed.cache_tokens as f64 / ahead_changed.input_tokens.max(1) as f64;
    println!(
        "after a plan change: context last reused {:.0}% of the prompt, context ahead {:.0}%",
        last_share * 100.0,
        ahead_share * 100.0
    );
    if last_changed.cache_tokens <= ahead_changed.cache_tokens {
        eprintln!("context last did not keep more of the history cached than context ahead");
        std::process::exit(1);
    }
}
