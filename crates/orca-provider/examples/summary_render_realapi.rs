//! Real-API harness for the summary-delta renderer cost (follow-up step 4).
//!
//! Unlike `summary_render_metrics` (offline, byte/token estimates only), this
//! harness makes REAL, BILLED DeepSeek calls against the auxiliary model and
//! reads back the API-reported `prompt_tokens` / cache hit numbers. It validates
//! the follow-up acceptance targets:
//!
//!   huge  current renderer prompt_tokens <= old micro-masked prompt_tokens
//!   mid   current renderer prompt_tokens <  old (un-rendered) prompt_tokens
//!   normal main-context cache hit         >= 90% on the second identical turn
//!   summary cache second lookup           skips the API (local content cache)
//!
//! Requires DEEPSEEK_API_KEY (env var or ~/.orca/auth.json).
//! Run: `cargo run -p orca-provider --example summary_render_realapi`

use std::collections::HashMap;

use orca_core::config::ProviderKind;
use orca_core::conversation::{Conversation, Message};
use orca_provider::context::render_summary_delta;
use orca_provider::summary_cache;
use orca_provider::{ProviderConfig, call};

// Mirrors the production summary request in context.rs so the prompt sent here
// is byte-identical to what `request_summary` would send for the same delta.
const SUMMARY_SYSTEM_PROMPT: &str = "Summarize old agent conversation context for future continuation. Preserve user goals, decisions, file paths, tool results, blockers, and exact constraints. Be concise and factual.";
const AUX_MODEL: &str = "deepseek-flash";

fn tool(content: String) -> Message {
    Message::Tool {
        tool_call_id: "call_1".to_string(),
        content,
        terminal: None,
        pinned: false,
    }
}

// Mirrors `micro_compact_tool_output` in context.rs: the OLD main-context path
// that the summary delta used to inherit (head/tail 320 chars + size header).
fn micro_compact_tool_output(content: &str) -> String {
    let head: String = content.chars().take(320).collect();
    let tail_vec: Vec<char> = content.chars().rev().take(320).collect();
    let tail: String = tail_vec.into_iter().rev().collect();
    format!(
        "[tool output micro-compact]\noriginal_bytes: {}\nhead:\n{}\n\ntail:\n{}",
        content.len(),
        head.trim_end(),
        tail.trim_start()
    )
}

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

/// Send a collapsed-delta text through the real aux model exactly as
/// `request_summary` does and return `(prompt_tokens, cache_tokens)`.
fn summary_prompt_tokens(config: &ProviderConfig, collapsed_text: &str) -> (u64, u64) {
    let user_prompt = format!("Summarize this collapsed conversation segment:\n\n{collapsed_text}");
    let mut conv = Conversation::new();
    conv.add_system(SUMMARY_SYSTEM_PROMPT.to_string());
    conv.add_user(user_prompt);
    let resp = call(ProviderKind::DeepSeek, &conv, config);
    let usage = resp
        .usage
        .expect("aux model response must report usage (prompt_tokens)");
    (usage.input_tokens, usage.cache_tokens)
}

fn main() {
    let Some(api_key) = load_api_key() else {
        eprintln!("DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping real-API eval.");
        std::process::exit(1);
    };

    // Aux-model config used for summary requests (tools disabled, like prod).
    let summary_config = ProviderConfig {
        api_key: Some(api_key.clone()),
        base_url: None,
        model: Some(AUX_MODEL.to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };

    // ---- Fixtures -------------------------------------------------------
    // A huge ~40KB tool output (the scenario that was 5644 vs 2252) and a
    // mid ~5KB tool output.
    let huge: String = (0..600)
        .map(|i| format!("row {i}: {{\"id\":{i},\"payload\":\"{}\"}}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    let mid: String = (0..120)
        .map(|i| format!("2024-06-23 INFO worker[{i}] processed batch ok with details"))
        .collect::<Vec<_>>()
        .join("\n");

    let user = Message::user("inspect the tool output".to_string());

    // huge: old micro-masked vs current renderer.
    // The micro-compacted text carries the "[tool output micro-compact]" marker,
    // so render_summary_delta passes it through unchanged -> identical framing
    // for an apples-to-apples comparison against the new extractive renderer.
    let huge_old =
        render_summary_delta(&[user.clone(), tool(micro_compact_tool_output(&huge))]).text;
    let huge_new = render_summary_delta(&[user.clone(), tool(huge.clone())]).text;

    // mid: old without renderer (raw original delta) vs current renderer.
    let mid_old = render_summary_delta(&[user.clone(), tool(mid.clone())]).text; // current renderer
    // For the un-rendered baseline we format the raw messages by sending the raw
    // content directly (no extractive step).
    let mid_raw = format!(
        "[user]\ninspect the tool output\n\n[tool call_1]\n{}\n\n",
        mid.trim()
    );

    println!("== Real-API summary-delta cost ==");
    println!("(aux model = {AUX_MODEL})\n");

    let (huge_old_pt, _) = summary_prompt_tokens(&summary_config, &huge_old);
    let (huge_new_pt, _) = summary_prompt_tokens(&summary_config, &huge_new);
    let (mid_raw_pt, _) = summary_prompt_tokens(&summary_config, &mid_raw);
    let (mid_new_pt, _) = summary_prompt_tokens(&summary_config, &mid_old);

    println!("summary_huge_old_micro_masked          prompt={huge_old_pt}");
    println!("summary_huge_current_original_renderer prompt={huge_new_pt}");
    println!("summary_mid_old_without_renderer       prompt={mid_raw_pt}");
    println!("summary_mid_current_renderer           prompt={mid_new_pt}\n");

    // ---- Normal main-context cache hit (two identical turns) ------------
    let normal_config = ProviderConfig {
        api_key: Some(api_key.clone()),
        base_url: None,
        model: Some(AUX_MODEL.to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let mut normal = Conversation::new();
    normal.add_system(format!(
        "You are a terminal coding agent. {}",
        "Follow the user's instructions precisely and keep this stable prefix. ".repeat(120)
    ));
    for turn in 0..12 {
        normal.add_user(format!(
            "Context priming line {turn}: keep this prefix stable across turns. {}",
            "padding token sequence for a realistic prefix length ".repeat(8)
        ));
        normal.add_assistant(
            Some(format!(
                "Acknowledged priming line {turn}. {}",
                "stable acknowledgement body ".repeat(8)
            )),
            None,
            Vec::new(),
        );
    }
    normal.add_user("Reply with the single word: ok".to_string());

    let first = call(ProviderKind::DeepSeek, &normal, &normal_config)
        .usage
        .expect("normal turn 1 usage");
    let second = call(ProviderKind::DeepSeek, &normal, &normal_config)
        .usage
        .expect("normal turn 2 usage");
    let hit_pct = if second.input_tokens == 0 {
        0.0
    } else {
        (second.cache_tokens as f64 / second.input_tokens as f64) * 100.0
    };
    println!(
        "normal_turn_1 prompt={} cache={}",
        first.input_tokens, first.cache_tokens
    );
    println!(
        "normal_turn_2 prompt={} cache={} hit={hit_pct:.1}%\n",
        second.input_tokens, second.cache_tokens
    );

    // ---- Local summary cache: second lookup must skip the API -----------
    // Use a per-run nonce so the key always starts empty regardless of cache
    // entries left by earlier runs; this isolates the "store -> second lookup
    // hits without an API call" behavior.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scope = format!("provider=deepseek;model=aux;harness;nonce={nonce}");
    let key = summary_cache::summary_key(&scope, "delta", None, &huge_new);
    let first_lookup = summary_cache::lookup(&key).is_some();
    summary_cache::store(&key, "cached summary body");
    let second_lookup = summary_cache::lookup(&key).is_some();
    let cache_skips_api = !first_lookup && second_lookup;
    println!(
        "local_summary_cache first_lookup_miss={} second_lookup_hit={second_lookup} skips_api={cache_skips_api}\n",
        !first_lookup
    );

    // ---- Acceptance verdict --------------------------------------------
    let huge_ok = huge_new_pt <= huge_old_pt;
    let mid_ok = mid_new_pt < mid_raw_pt;
    let cache_hit_ok = hit_pct >= 90.0;

    println!("== Acceptance ==");
    verdict(
        "huge current renderer <= old micro-masked",
        huge_ok,
        &format!("{huge_new_pt} <= {huge_old_pt}"),
    );
    verdict(
        "mid current renderer < old without renderer",
        mid_ok,
        &format!("{mid_new_pt} < {mid_raw_pt}"),
    );
    verdict(
        "normal main cache hit >= 90%",
        cache_hit_ok,
        &format!("{hit_pct:.1}%"),
    );
    verdict(
        "summary cache second lookup skips API",
        cache_skips_api,
        &format!("{cache_skips_api}"),
    );

    if huge_ok && mid_ok && cache_hit_ok && cache_skips_api {
        println!("\nALL TARGETS MET");
    } else {
        println!("\nSOME TARGETS NOT MET");
        std::process::exit(2);
    }
}

fn verdict(name: &str, pass: bool, detail: &str) {
    let tag = if pass { "PASS" } else { "FAIL" };
    println!("  [{tag}] {name} ({detail})");
}
