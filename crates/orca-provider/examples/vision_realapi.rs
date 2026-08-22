//! Real DeepSeek vision smoke.
//!
//! Loads `DEEPSEEK_API_KEY` from the environment or `~/.orca/auth.json`.

use std::fs;

use orca_core::config::{ProviderKind, ReasoningEffort};
use orca_core::conversation::{Conversation, ImageDetail, ImageInput, ImageSource};
use orca_provider::ProviderConfig;

fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    let path = dirs::home_dir()?.join(".orca/auth.json");
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    value
        .get("DEEPSEEK_API_KEY")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn main() {
    let Some(api_key) = api_key() else {
        eprintln!("DEEPSEEK_API_KEY not found; skipping vision smoke.");
        return;
    };
    let mut conversation = Conversation::new();
    conversation.add_user_with_images(
        "Reply with a short description of this image.".to_string(),
        vec![ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string(),
            },
            detail: ImageDetail::High,
        }],
    );
    let response = orca_provider::call(
        ProviderKind::DeepSeek,
        &conversation,
        &ProviderConfig {
            api_key: Some(api_key),
            base_url: None,
            model: Some(orca_core::model::VISION_MODEL.to_string()),
            reasoning_effort: ReasoningEffort::Low,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        },
    );
    if let Some(error) = response.error() {
        panic!("vision smoke failed: {error}");
    }
    let content = response
        .assistant_content
        .as_deref()
        .map(str::trim)
        .filter(|content| !content.is_empty())
        .expect("vision response must contain assistant content");
    println!(
        "vision smoke ok: response_chars={} input_tokens={} output_tokens={}",
        content.chars().count(),
        response.usage.map_or(0, |usage| usage.input_tokens),
        response.usage.map_or(0, |usage| usage.output_tokens),
    );
}
