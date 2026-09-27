//! Minimal, deterministic harness for the remote-summary input pipeline.
//!
//! It exercises `render_summary_delta` (the single summary-delta renderer) on a
//! set of fixed scenarios and prints the `remote_summary.*` metrics. No network
//! calls are made; the numbers are fully reproducible across runs and machines,
//! so they can anchor regression checks for the Round 7 acceptance targets:
//!
//!   1. mid-sized tool output  -> sustained 25-40% byte reduction
//!   2. huge tool output       -> no longer 0%, at least 30% reduction
//!   3. already-compacted input -> 0% (not double-compacted)
//!
//! Run with: `cargo run -p orca-provider --example summary_render_metrics`

use orca_core::conversation::Message;
use orca_provider::context::render_summary_delta;

fn tool(content: String) -> Message {
    Message::Tool {
        tool_call_id: "call_1".to_string(),
        content,
        images: Vec::new(),
        terminal: None,
        pinned: false,
    }
}

fn scenario(name: &str, messages: &[Message]) {
    let rendered = render_summary_delta(messages);
    let byte_drop = pct(rendered.original_bytes, rendered.rendered_bytes);
    let token_drop = pct(rendered.original_tokens_est, rendered.rendered_tokens_est);
    println!("--- {name} ---");
    println!(
        "  remote_summary.input_bytes        = {}",
        rendered.original_bytes
    );
    println!(
        "  remote_summary.rendered_bytes     = {}",
        rendered.rendered_bytes
    );
    println!(
        "  remote_summary.input_tokens_est   = {}",
        rendered.original_tokens_est
    );
    println!(
        "  remote_summary.rendered_tokens_est= {}",
        rendered.rendered_tokens_est
    );
    println!(
        "  remote_summary.compacted_outputs  = {}",
        rendered.compacted_tool_outputs
    );
    println!("  byte_reduction                    = {byte_drop:.1}%");
    println!("  token_reduction                   = {token_drop:.1}%");
}

fn pct(original: usize, rendered: usize) -> f64 {
    if original == 0 {
        return 0.0;
    }
    (1.0 - (rendered as f64 / original as f64)) * 100.0
}

fn main() {
    // 1. Mid-sized multi-line tool output (~3KB log dump).
    let mid: String = (0..120)
        .map(|i| format!("2024-06-23 INFO worker[{i}] processed batch ok"))
        .collect::<Vec<_>>()
        .join("\n");
    scenario(
        "mid-sized tool output",
        &[
            Message::user("inspect the worker log".to_string()),
            tool(mid),
        ],
    );

    // 2. Huge tool output (~40KB), the scenario that previously showed 0% drop
    //    because main-context micro compaction masked the extractive rules.
    let huge: String = (0..600)
        .map(|i| format!("row {i}: {{\"id\":{i},\"payload\":\"{}\"}}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    scenario("huge tool output", &[tool(huge)]);

    // 3. Already micro-compacted input must not be compacted a second time.
    let already = format!(
        "[tool output micro-compact]\noriginal_bytes: 99999\nhead:\n{}\n\ntail:\n{}",
        "h".repeat(400),
        "t".repeat(400)
    );
    scenario("already-compacted tool output", &[tool(already)]);

    // 4. Natural-language only segment: nothing should be compacted.
    scenario(
        "natural-language only",
        &[
            Message::user("user intent ".repeat(50)),
            Message::Assistant {
                content: Some("assistant decision ".repeat(50)),
                reasoning_content: None,
                tool_calls: vec![],
                pinned: false,
            },
        ],
    );
}
