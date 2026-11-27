use super::*;
use crate::prompt_cache::measure_deepseek_prefix_reuse;
use orca_core::conversation::{ImageSource, RawToolCall};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn provider() -> ProviderConfig {
    ProviderConfig {
        api_key: None,
        base_url: None,
        model: None,
        reasoning_effort: Default::default(),
        tools_override: Some(vec![]),
        mcp_registry: None,
        external_tools: vec![],
        max_output_tokens: None,
    }
}

fn config(soft: usize, hard: usize) -> ContextConfig {
    ContextConfig {
        max_tokens: hard,
        compaction_threshold: 1.0,
        reserved_for_response: 0,
        auto_compact_token_limit: Some(hard),
        soft_compact_token_limit: Some(soft),
    }
}

fn tool_turn(conversation: &mut Conversation, index: usize, words: usize) {
    conversation.add_user(format!("inspect file {index}"));
    conversation.add_assistant(
        None,
        Some("inspect the evidence".to_string()),
        vec![RawToolCall {
            id: format!("call-{index}"),
            function_name: "read_file".to_string(),
            arguments: format!(r#"{{"path":"file-{index}.rs"}}"#),
        }],
    );
    conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(words));
}

fn tool_fixture() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    conversation.add_system("immutable repository constraints".to_string());
    for index in 0..6 {
        tool_turn(&mut conversation, index, 1_000);
    }
    conversation.add_user("current request".to_string());
    conversation
}

#[test]
fn deep_compaction_keeps_a_fork_childs_section_updates() {
    use orca_core::conversation::{
        ProviderForkPrefix, SYSTEM_PROMPT_RECORD_VERSION, SystemPromptRecord, SystemPromptSection,
    };

    let update = |name: &str, text: Option<&str>| {
        Message::system_with_prompt_record(
            format!("{name}: {}", text.unwrap_or("removed")),
            SystemPromptRecord {
                version: SYSTEM_PROMPT_RECORD_VERSION,
                sections: vec![SystemPromptSection {
                    name: name.into(),
                    text: text.map(str::to_owned),
                    instruction_scope: None,
                }],
                update: true,
                resume_boundary: None,
                redacted: false,
            },
        )
    };
    let mut child = Conversation::new();
    child.fork_prefix = Some(Arc::new(ProviderForkPrefix {
        model: orca_core::model::PRO_MODEL.into(),
        endpoint: deepseek_http::DEFAULT_BASE_URL.into(),
        reasoning_effort: Default::default(),
        messages: vec![r#"{"role":"system","content":"parent instructions"}"#.into()],
        tools: vec![],
        estimated_tokens: 8,
        system_messages: vec![],
        redacted: false,
    }));
    child.add_user("old request ".repeat(10));
    child.messages.extend([
        update("environment", Some("earlier shell")),
        update("environment", Some("current shell")),
        update("project instructions", None),
        update("cwd", Some("earlier workspace")),
    ]);
    child.add_user("latest request ".repeat(50));
    child
        .messages
        .push(update("cwd", Some("current workspace")));

    let config = config(110, 256);
    let summarized = compact_with_summary(ProviderKind::DeepSeek, &child, &config, &provider());
    assert!(matches!(summarized.kind, CompactionKind::RemoteSummary(_)));
    for compacted in [compact(&child, &config), summarized.conversation] {
        assert_eq!(compacted.fork_prefix, child.fork_prefix);
        let updates = compacted
            .messages
            .iter()
            .filter(|message| message.is_system_prompt_update())
            .filter_map(Message::content_str)
            .collect::<Vec<_>>();
        assert_eq!(
            updates,
            [
                "environment: current shell",
                "project instructions: removed",
                "cwd: current workspace"
            ]
        );
        let summary = compacted.summary.baseline.unwrap();
        assert!(!compacted.messages.iter().any(|message| {
            message
                .content_str()
                .is_some_and(|text| text.starts_with("old request"))
        }));
        assert!(!summary.contains("environment:") && !summary.contains("project instructions:"));
    }

    child.fork_prefix = None;
    child
        .messages
        .insert(0, Message::system("old instructions".into()));
    child.compaction_system_prompt = Some(Message::system("current instructions".into()));
    let compacted = compact(&child, &config);
    assert_eq!(
        compacted.messages[0].content_str(),
        Some("current instructions")
    );
    assert!(
        !compacted
            .messages
            .iter()
            .any(Message::is_system_prompt_update)
    );
}

#[test]
fn cache_aware_reduction_preserves_more_prefix_than_eager_micro_ab() {
    let before = tool_fixture();
    let provider = provider();
    let tokens = wire_equivalent_tokens(&before, &provider);
    let config = config(tokens - 50, tokens + 100);
    assert!(context_pressure(&before, &config, &provider).should_soft_compact);
    assert!(!context_pressure(&before, &config, &provider).should_hard_compact);
    let mut eager = before.clone();
    for message in &mut eager.messages {
        if let Message::Tool { content, .. } = message {
            *content = micro_compact_tool_output(content, None);
        }
    }
    let result = compact_with_summary(ProviderKind::DeepSeek, &before, &config, &provider);
    assert!(matches!(result.kind, CompactionKind::LocalTruncation));
    let a = measure_deepseek_prefix_reuse(&before, &provider, &eager, &provider).unwrap();
    let b =
        measure_deepseek_prefix_reuse(&before, &provider, &result.conversation, &provider).unwrap();
    assert!(b.unchanged_prefix_messages > a.unchanged_prefix_messages);
    assert!(b.unchanged_prefix_tokens_est > a.unchanged_prefix_tokens_est);
    assert!(b.after_tokens_est <= config.soft_limit() * 9 / 10);
    assert_eq!(result.conversation.messages.len(), before.messages.len());
    println!(
        "prefix_ab A={} B={}",
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );

    let again = compact_with_summary(
        ProviderKind::DeepSeek,
        &result.conversation,
        &config,
        &provider,
    );
    let metrics = measure_deepseek_prefix_reuse(
        &result.conversation,
        &provider,
        &again.conversation,
        &provider,
    )
    .unwrap();
    assert_eq!(metrics.before_tokens_est, metrics.after_tokens_est);
    assert_eq!(
        metrics.unchanged_prefix_messages,
        crate::deepseek_http::conversation_to_api_messages(&again.conversation).len()
    );
}

#[test]
fn below_pressure_keeps_exact_prefix_and_dynamic_overlay_trails_history() {
    let mut conversation = tool_fixture();
    conversation.summary.baseline = Some("stable summary".to_string());
    conversation.replace_plan_state("first plan".to_string());
    let provider = provider();
    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &ContextConfig::default(),
        &provider,
    );
    let metrics =
        measure_deepseek_prefix_reuse(&conversation, &provider, &result.conversation, &provider)
            .unwrap();
    assert_eq!(metrics.before_tokens_est, metrics.after_tokens_est);
    let mut changed = result.conversation.clone();
    changed.replace_plan_state("second plan".to_string());
    let metrics =
        measure_deepseek_prefix_reuse(&conversation, &provider, &changed, &provider).unwrap();
    assert_eq!(
        metrics.unchanged_prefix_messages,
        crate::deepseek_http::conversation_to_api_messages(&changed).len() - 1
    );
    let changed_provider = ProviderConfig {
        model: Some("another-model".to_string()),
        ..provider.clone()
    };
    assert_eq!(
        measure_deepseek_prefix_reuse(&conversation, &provider, &conversation, &changed_provider)
            .unwrap()
            .unchanged_prefix_messages,
        0
    );
}

#[test]
fn hard_pressure_keeps_pinned_units_and_the_current_turn_tail() {
    let mut conversation = tool_fixture();
    if let Message::Tool { pinned, .. } = &mut conversation.messages[4] {
        *pinned = true;
    }
    tool_turn(&mut conversation, 99, 600);
    conversation.add_assistant(
        None,
        None,
        vec![
            RawToolCall {
                id: "pending-a".to_string(),
                function_name: "bash".to_string(),
                arguments: "{}".to_string(),
            },
            RawToolCall {
                id: "pending-b".to_string(),
                function_name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
        ],
    );
    conversation.add_tool_result("pending-b".to_string(), "completed b".to_string());
    let config = config(500, 700);
    let result = compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider());
    let messages = &result.conversation.messages;
    // The message that opened the current turn stays, and so does its newest unit.
    assert!(
        messages
            .iter()
            .any(|message| message.content_str() == Some("inspect file 99"))
    );
    assert!(messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, terminal: Some(_), .. } if tool_call_id == "pending-a")));
    // The pinned tool result keeps the call that produced it.
    assert!(messages.iter().any(|message| matches!(message,
        Message::Assistant { tool_calls, .. } if tool_calls.iter().any(|call| call.id == "call-0"))));
    assert!(
        !messages
            .iter()
            .any(|message| message.content_str() == Some("inspect file 2"))
    );
}

#[test]
fn pinned_notices_in_every_turn_do_not_block_compaction() {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    for index in 0..6 {
        tool_turn(&mut conversation, index, 1_000);
        conversation.add_system_pinned(format!(
            "<task-notification>task {index} finished</task-notification>"
        ));
    }
    conversation.add_user("current request".to_string());
    let before = conversation.messages.len();

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(500, 700),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(messages.len() < before, "nothing was compacted");
    for index in 0..6 {
        let notice = format!("<task-notification>task {index} finished</task-notification>");
        assert!(
            messages.iter().any(
                |message| message.is_pinned() && message.content_str() == Some(notice.as_str())
            )
        );
    }
    assert!(!messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-0")));
    assert!(
        messages
            .iter()
            .any(|message| message.content_str() == Some("current request"))
    );

    // Pinned messages are kept conversation content, not instructions: they
    // must sort after the summary the compaction injects, not before it.
    let api_messages = crate::deepseek_http::conversation_to_api_messages(&result.conversation);
    let summary_index = api_messages
        .iter()
        .position(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("\n\n[Summary baseline]\n"))
        })
        .expect("compaction must produce a summary baseline");
    let first_notice_index = api_messages
        .iter()
        .position(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("<task-notification>"))
        })
        .expect("pinned notices must survive compaction");
    assert!(
        summary_index < first_notice_index,
        "pinned messages must sort after the summary, not before it"
    );
}

#[test]
fn compaction_cuts_into_a_long_current_turn() {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    conversation.add_user("refactor the whole module".to_string());
    for index in 0..30 {
        conversation.add_assistant(
            None,
            Some("next step".to_string()),
            vec![RawToolCall {
                id: format!("call-{index}"),
                function_name: "read_file".to_string(),
                arguments: format!(r#"{{"path":"file-{index}.rs"}}"#),
            }],
        );
        conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(200));
    }

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(2_000, 3_000),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(
        messages.len() < conversation.messages.len(),
        "nothing was compacted"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.content_str() == Some("refactor the whole module"))
    );
    assert!(messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-29")));
    assert!(!messages.iter().any(|message| matches!(message,
        Message::Tool { tool_call_id, .. } if tool_call_id == "call-0")));
    // Every kept tool result still follows the call that asked for it.
    for (index, message) in messages.iter().enumerate() {
        if let Message::Tool { tool_call_id, .. } = message {
            assert!(messages[..index].iter().any(|earlier| matches!(earlier,
                Message::Assistant { tool_calls, .. }
                    if tool_calls.iter().any(|call| &call.id == tool_call_id))));
        }
    }
}

#[test]
fn micro_compaction_shortens_output_beside_a_pinned_notice() {
    let mut conversation = tool_fixture();
    // Right after the first turn's tool result.
    conversation.messages.insert(
        5,
        Message::pinned_system("<task-notification>done</task-notification>".to_string()),
    );

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(1_500, 20_000),
        &provider(),
    );
    let messages = &result.conversation.messages;

    assert!(matches!(&messages[4],
        Message::Tool { content, .. } if content.starts_with("[tool output micro-compact]")));
    assert!(matches!(&messages[5],
        Message::System { content, pinned: true, .. } if content.contains("done")));
}

#[test]
fn micro_compaction_exempts_the_current_turn_openers_images() {
    let mut conversation = Conversation::new();
    conversation.add_system("instructions".to_string());
    // Earlier history with a large, shrinkable tool output: pressure to
    // relieve without ever touching the current turn's images.
    tool_turn(&mut conversation, 0, 5_000);
    let image = ImageInput {
        source: ImageSource::File {
            file_id: "evidence".to_string(),
        },
        detail: ImageDetail::Auto,
    };
    conversation.add_user_with_images("describe these".to_string(), vec![image; 5]);
    conversation.add_assistant(
        None,
        Some("looking".to_string()),
        vec![RawToolCall {
            id: "call-1".to_string(),
            function_name: "read_file".to_string(),
            arguments: r#"{"path":"a.rs"}"#.to_string(),
        }],
    );
    conversation.add_tool_result("call-1".to_string(), "small result".to_string());

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(13_000, 30_000),
        &provider(),
    );
    let messages = &result.conversation.messages;

    // The micro path (not the deep/local-truncation fallback) must have run
    // and been accepted: the early tool turn is still a live message, only
    // shortened in place, not collapsed away by a partition.
    assert!(
        messages
            .iter()
            .any(|message| message.content_str() == Some("inspect file 0"))
    );
    // The current turn's opener keeps its images and its text untouched.
    assert!(
        messages.iter().any(|message| matches!(message,
        Message::User { content, images, .. } if content == "describe these" && images.len() == 5))
    );
}

fn tool_output<'a>(conversation: &'a Conversation, id: &str) -> &'a str {
    conversation
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Tool {
                tool_call_id,
                content,
                ..
            } if tool_call_id == id => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} has no tool result"))
}

fn shortened_outputs(conversation: &Conversation) -> usize {
    conversation
        .messages
        .iter()
        .filter(|message| {
            matches!(message,
            Message::Tool { content, .. } if content.starts_with("[tool output micro-compact]"))
        })
        .count()
}

#[test]
fn micro_compaction_keeps_the_current_turns_recent_tool_output() {
    let mut conversation = Conversation::new();
    conversation.add_system("immutable instructions".to_string());
    conversation.add_user("refactor the whole module".to_string());
    for index in 0..8 {
        conversation.add_assistant(
            None,
            Some("next step".to_string()),
            vec![RawToolCall {
                id: format!("call-{index}"),
                function_name: "read_file".to_string(),
                arguments: format!(r#"{{"path":"file-{index}.rs"}}"#),
            }],
        );
        conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(1_000));
    }
    let messages = &conversation.messages;
    let unit = message_tokens(&messages[messages.len() - 2])
        + message_tokens(&messages[messages.len() - 1]);
    // The retention budget, 6/10 of the soft line, holds three and a half of
    // these units: the three newest are the turn's recent tail.
    let soft = unit * 35 / 6;

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(soft, soft * 2),
        &provider(),
    );

    assert!(matches!(result.kind, CompactionKind::LocalTruncation));
    assert_eq!(
        result.conversation.messages.len(),
        conversation.messages.len()
    );
    let full = "evidence ".repeat(1_000);
    for id in ["call-5", "call-6", "call-7"] {
        assert!(
            tool_output(&result.conversation, id) == full,
            "{id} is in the recent tail"
        );
    }
    // Older units of the same turn are shortened, newest first.
    assert!(tool_output(&result.conversation, "call-4").starts_with("[tool output micro-compact]"));
}

#[test]
fn calibrated_micro_compaction_stops_once_the_scaled_estimate_is_under_target() {
    let mut conversation = tool_fixture();
    let provider = provider();
    let estimated = wire_equivalent_tokens(&conversation, &provider);
    // The provider counted half again the estimate, a ratio the anchor accepts.
    conversation.record_usage_anchor(
        (estimated * 3 / 2) as u64,
        estimated,
        conversation.messages.len(),
    );
    let ratio = measure_prompt(&conversation, &provider).tokens as f64 / estimated as f64;
    // Every tool output is the same size, so each shortening saves the same.
    let mut shortened = conversation.clone();
    let last_output = shortened
        .messages
        .iter_mut()
        .rev()
        .find_map(|message| match message {
            Message::Tool { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    *last_output = micro_compact_tool_output(last_output, None);
    let saving = estimated - wire_equivalent_tokens(&shortened, &provider);
    // The micro target (9/10 of the soft line) sits halfway between two and
    // three shortened outputs on the provider's scale.
    let target = ((estimated as f64 - 2.5 * saving as f64) * ratio) as usize;
    let soft = target * 10 / 9 + 1;

    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(soft, soft * 2),
        &provider,
    );

    assert!(matches!(result.kind, CompactionKind::LocalTruncation));
    assert_eq!(shortened_outputs(&result.conversation), 3);
}

#[test]
fn pressure_keeps_the_anchor_when_normalization_repairs_earlier_history() {
    let mut conversation = tool_fixture();
    // An early call whose result never arrived: normalization repairs it with
    // an inserted result, which moves every later message up by one.
    conversation.messages.insert(
        3,
        Message::Assistant {
            cut_short: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![RawToolCall {
                id: "lost".to_string(),
                function_name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
            pinned: false,
        },
    );
    let provider = provider();
    let estimated = wire_equivalent_tokens(&conversation, &provider);
    // The provider counted half again the estimate, a ratio the anchor accepts.
    conversation.record_usage_anchor(
        (estimated * 3 / 2) as u64,
        estimated,
        conversation.messages.len(),
    );
    assert_eq!(
        measure_prompt(&conversation, &provider).tokens,
        estimated * 3 / 2
    );

    // Past the line only on the provider's count.
    let soft = estimated * 5 / 4;
    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(soft, soft * 2),
        &provider,
    );

    assert!(shortened_outputs(&result.conversation) > 0);
}

#[test]
fn image_detail_budgets_apply_without_tokenizing_payload_and_keep_current_images() {
    let mut image = ImageInput {
        source: ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "A".repeat(100_000),
        },
        detail: ImageDetail::Low,
    };
    for (detail, expected) in [
        (ImageDetail::Low, 384),
        (ImageDetail::High, 2_048),
        (ImageDetail::Auto, 2_048),
        (ImageDetail::Original, 4_096),
    ] {
        image.detail = detail;
        assert_eq!(image_tokens(&image), expected);
    }
    let mut conversation = Conversation::new();
    conversation.add_system("system".to_string());
    conversation.add_user_with_images("old image".to_string(), vec![image.clone(); 4]);
    conversation.add_assistant(Some("old image was red".to_string()), None, vec![]);
    conversation.add_user_with_images("compare current".to_string(), vec![image.clone()]);
    let result = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config(6_000, 8_000),
        &provider(),
    );
    assert!(wire_equivalent_tokens(&result.conversation, &provider()) < 6_000);
    assert!(
        matches!(result.conversation.messages.last(), Some(Message::User { images, .. }) if images == &vec![image])
    );
    assert!(result.conversation.messages.iter().any(|message| {
        message
            .content_str()
            .is_some_and(|text| text.contains("historical image input(s) omitted"))
    }));
}

#[test]
fn historical_image_budget_prefers_recent_evidence_and_replayed_reasoning_is_counted() {
    let image = ImageInput {
        source: ImageSource::File {
            file_id: "image-evidence".to_string(),
        },
        detail: ImageDetail::Low,
    };
    let mut conversation = Conversation::new();
    conversation.add_user_with_images("older image".to_string(), vec![image.clone()]);
    conversation.add_user_with_images("recent image".to_string(), vec![image]);
    conversation.add_user("current".to_string());
    let reduced = cache_aware_micro_compaction(&conversation, 0, 1_536, 1.0, conversation_tokens);
    assert!(matches!(&reduced.messages[0], Message::User { images, .. } if images.is_empty()));
    assert!(matches!(&reduced.messages[1], Message::User { images, .. } if images.len() == 1));
    let mut tool = Conversation::new();
    tool_turn(&mut tool, 0, 1);
    let without = conversation_tokens(&tool);
    if let Message::Assistant {
        reasoning_content, ..
    } = &mut tool.messages[1]
    {
        *reasoning_content = Some("reasoning ".repeat(1_000));
    }
    assert!(conversation_tokens(&tool) > without + 900);
}

#[test]
fn no_system_and_huge_current_request_are_not_mistaken_for_discardable_history() {
    let mut conversation = Conversation::new();
    conversation.add_user("old goal".to_string());
    conversation.add_assistant(Some("old answer".to_string()), None, vec![]);
    let current = "current constraint ".repeat(5_000);
    conversation.add_user(current.clone());
    let result = compact_with_summary(
        ProviderKind::Mock,
        &conversation,
        &config(100, 200),
        &provider(),
    );
    assert_eq!(
        result.conversation.messages.last().unwrap().content_str(),
        Some(current.as_str())
    );
    assert!(
        result
            .conversation
            .summary
            .baseline
            .as_deref()
            .unwrap()
            .contains("old goal")
    );
    let api = crate::deepseek_http::conversation_to_api_messages(&result.conversation);
    assert_eq!(api[0].role, "system");
    assert_eq!(
        api.last().unwrap().content.as_deref(),
        Some(current.as_str())
    );
}

#[test]
fn summary_chunks_are_bounded_lossless_and_marker_text_cannot_bypass_bounds() {
    let text = format!(
        "{}\n\nMIDDLE_REQUIRED_FACT\n\n{}",
        "first evidence ".repeat(3_000),
        "last evidence ".repeat(3_000)
    );
    let chunks = summary_chunks(&text);
    assert!(chunks.len() > 1);
    assert_eq!(chunks.concat(), text);
    assert!(
        chunks
            .iter()
            .all(|chunk| DefaultTokenCounter.count_text(chunk) <= SUMMARY_INPUT_TOKENS)
    );
    let huge = format!("[extractive-compact] {}", "long tool data ".repeat(8_000));
    assert!(render_tool_output(&huge).0.len() <= SUMMARY_HUGE_MAX_BYTES);
    let rendered = render_summary_evidence(&[Message::user(text)]);
    assert!(rendered.contains("MIDDLE_REQUIRED_FACT"));
}

#[derive(Debug, Deserialize)]
struct SummaryRequest {
    messages: Vec<SummaryMessage>,
    max_tokens: u32,
    thinking: SummaryThinking,
    model: String,
    reasoning_effort: Option<String>,
    tools: Vec<serde_json::Value>,
    #[serde(skip)]
    wire_messages: Vec<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct SummaryMessage {
    role: String,
    #[serde(default)]
    content: serde_json::Value,
    reasoning_content: Option<String>,
    tool_calls: Option<serde_json::Value>,
    tool_call_id: Option<String>,
}

impl SummaryMessage {
    fn text(&self) -> &str {
        self.content.as_str().expect("expected text content")
    }
}

#[derive(Debug, Deserialize)]
struct SummaryThinking {
    #[serde(rename = "type")]
    kind: String,
}

struct SummaryServer {
    base_url: String,
    requests: Arc<Mutex<Vec<SummaryRequest>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SummaryServer {
    fn new(status: u16, finish_reason: &'static str) -> Self {
        Self::with_reply(status, move |_| {
            serde_json::json!({
                "choices": [{"delta": {"content": "Preserve MIDDLE_REQUIRED_FACT and pending work."}, "finish_reason": finish_reason}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 12, "prompt_cache_hit_tokens": 64}
            })
        })
    }

    fn with_reply(
        status: u16,
        reply: impl Fn(&SummaryRequest) -> serde_json::Value + Send + 'static,
    ) -> Self {
        Self::with_status_and_reply(move |request| (status, reply(request)))
    }

    fn with_status_and_reply(
        reply: impl Fn(&SummaryRequest) -> (u16, serde_json::Value) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        let body = read_request(&mut stream);
                        #[derive(Deserialize)]
                        struct WireMessages {
                            messages: Vec<Box<serde_json::value::RawValue>>,
                        }
                        let mut request: SummaryRequest = serde_json::from_str(&body).unwrap();
                        request.wire_messages = serde_json::from_str::<WireMessages>(&body)
                            .unwrap()
                            .messages;
                        let (status, delta) = reply(&request);
                        let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                        captured.lock().unwrap().push(request);
                        write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("summary test server: {error}"),
                }
            }
        });
        Self {
            base_url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn config(&self) -> ProviderConfig {
        ProviderConfig {
            api_key: Some("test-only".to_string()),
            base_url: Some(self.base_url.clone()),
            ..provider()
        }
    }
}

impl Drop for SummaryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.worker.take().unwrap().join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = vec![];
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let mut buffer = [0; 4096];
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "summary fixture request timed out"
                );
                continue;
            }
            Err(error) => panic!("summary fixture request failed: {error}"),
        };
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
        assert!(
            bytes.len() <= 2 * 1024 * 1024,
            "summary fixture request exceeds bound"
        );
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            assert!(
                headers.starts_with("POST /chat/completions ")
                    || headers.starts_with("POST /beta/chat/completions ")
            );
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return String::from_utf8(bytes[end + 4..end + 4 + length].to_vec()).unwrap();
            }
        }
    }
}

#[test]
fn hierarchy_bounds_requests_preserves_middle_evidence_and_reuses_cached_results() {
    let server = SummaryServer::new(200, "stop");
    let mut conversation = Conversation::new();
    conversation.add_system("instructions".to_string());
    conversation.add_user(format!(
        "{}\n\nMIDDLE_REQUIRED_FACT\n\n{}",
        "first evidence ".repeat(3_000),
        "last evidence ".repeat(3_000)
    ));
    conversation.add_assistant(Some("acknowledged".to_string()), None, vec![]);
    conversation.add_user("current request".to_string());
    let config = config(2_500, 3_000);
    let first = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config,
        &server.config(),
    );
    assert!(matches!(first.kind, CompactionKind::RemoteSummary(_)));
    assert!(
        first
            .conversation
            .summary
            .baseline
            .as_deref()
            .unwrap()
            .contains("MIDDLE_REQUIRED_FACT")
    );
    assert!(wire_equivalent_tokens(&first.conversation, &server.config()) < config.soft_limit());
    let requests = server.requests.lock().unwrap();
    let count = requests.len();
    assert!((3..=MAX_SUMMARY_REQUESTS).contains(&count));
    assert!(
        requests
            .iter()
            .all(|request| request.max_tokens == 1_500 && request.thinking.kind == "disabled")
    );
    assert!(
        requests
            .iter()
            .all(|request| request.messages[0].role == "system"
                && DefaultTokenCounter.count_text(request.messages[1].text())
                    <= SUMMARY_INPUT_TOKENS + 100)
    );
    assert!(
        requests
            .iter()
            .any(|request| request.messages[1].text().contains("MIDDLE_REQUIRED_FACT"))
    );
    drop(requests);
    let again = compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &config,
        &server.config(),
    );
    assert_eq!(
        serde_json::to_value(&first.conversation.summary).unwrap(),
        serde_json::to_value(&again.conversation.summary).unwrap()
    );
    assert_eq!(server.requests.lock().unwrap().len(), count);
    assert_eq!(
        format_messages(&first.conversation.messages),
        format_messages(&again.conversation.messages)
    );
}

#[test]
fn failed_or_truncated_remote_summary_has_one_attempt_and_bounded_local_state() {
    for (status, finish) in [(503, "stop"), (200, "length")] {
        let server = SummaryServer::new(status, finish);
        let mut conversation = Conversation::new();
        conversation.add_system("instructions".to_string());
        conversation.add_user("old important constraint ".repeat(1_000));
        conversation.add_user("current request".to_string());
        conversation.summary.baseline = Some("durable baseline".to_string());
        let config = config(500, 700);
        let result = compact_with_summary(
            ProviderKind::DeepSeek,
            &conversation,
            &config,
            &server.config(),
        );
        assert!(matches!(result.kind, CompactionKind::LocalTruncation));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert!(summary_text_tokens(&result.conversation.summary) <= summary_budget(&config));
        assert!(
            summary_parts(&result.conversation.summary)
                .join(" ")
                .contains("durable baseline")
        );
        assert_eq!(
            result.conversation.messages.last().unwrap().content_str(),
            Some("current request")
        );
    }
}

#[test]
fn repeated_compaction_and_cancelled_rebuild_keep_bounded_durable_facts() {
    let server = SummaryServer::new(200, "stop");
    let config = config(600, 900);
    let mut conversation = Conversation::new();
    conversation.add_system("immutable".to_string());
    for index in 0..8 {
        conversation.add_user(format!("historical step {index} {}", "fact ".repeat(700)));
        conversation.add_assistant(Some("acknowledged".to_string()), None, vec![]);
        conversation.add_user(format!("current request {index}"));
        conversation = compact_with_summary(
            ProviderKind::DeepSeek,
            &conversation,
            &config,
            &server.config(),
        )
        .conversation;
        assert_eq!(conversation.messages[0].content_str(), Some("immutable"));
        assert!(summary_text_tokens(&conversation.summary) <= summary_budget(&config));
        assert!(conversation.summary.deltas.len() <= MAX_SUMMARY_DELTAS);
        assert!(wire_equivalent_tokens(&conversation, &server.config()) < config.soft_limit());
    }
    let count = server.requests.lock().unwrap().len();
    conversation.add_user("cancelled old evidence ".repeat(800));
    conversation.add_user("keep this active".to_string());
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = compact_with_summary_cancellable(
        ProviderKind::DeepSeek,
        &conversation,
        &config,
        &server.config(),
        &cancel,
    );
    assert!(matches!(result.kind, CompactionKind::LocalTruncation));
    assert_eq!(server.requests.lock().unwrap().len(), count);
    assert!(summary_text_tokens(&result.conversation.summary) <= summary_budget(&config));
    assert_eq!(
        result.conversation.messages.last().unwrap().content_str(),
        Some("keep this active")
    );
}

#[test]
fn hierarchical_work_limit_falls_back_without_unbounded_requests() {
    let text = "large input ".repeat(SUMMARY_INPUT_TOKENS * MAX_SUMMARY_REQUESTS);
    let mut left = 1;
    assert!(
        hierarchical_summary(
            ProviderKind::Mock,
            &provider(),
            SUMMARY_PURPOSE_DELTA,
            &text,
            128,
            None,
            &mut left
        )
        .is_none()
    );
    assert_eq!(left, 1);
}

#[test]
fn replay_messages_equal_the_next_request_up_to_the_cut() {
    let mut conversation = Conversation::new();
    conversation.add_system("system prompt".to_string());
    conversation.summary.baseline = Some("previous checkpoint".to_string());
    conversation
        .summary
        .deltas
        .push("recent summary update".to_string());
    tool_turn(&mut conversation, 0, 10);
    conversation.add_assistant(Some("evidence checked".to_string()), None, vec![]);
    conversation.add_user("next request".to_string());
    conversation.replace_plan_state("plan overlay".to_string());

    let next = crate::deepseek_http::conversation_to_api_messages(&conversation);
    let replay = replay_summary_messages(&conversation, 4);
    assert_eq!(replay.len(), 7);
    assert_eq!(
        serde_json::to_vec(&replay[..6]).unwrap(),
        serde_json::to_vec(&next[..6]).unwrap(),
    );
    assert_eq!(
        serde_json::to_value(&replay[6]).unwrap(),
        serde_json::json!({"role": "user", "content": REPLAY_SUMMARY_INSTRUCTION}),
    );
}

#[test]
fn replay_keeps_reasoning_content_as_the_next_request_would() {
    let mut conversation = Conversation::new();
    tool_turn(&mut conversation, 0, 10);
    conversation.add_assistant(
        Some("one finding".to_string()),
        Some("compare evidence".to_string()),
        vec![],
    );
    conversation.add_assistant(Some("another finding".to_string()), None, vec![]);

    let next = crate::deepseek_http::conversation_to_api_messages(&conversation);
    let replay = replay_summary_messages(&conversation, 4);
    for (index, reasoning) in [(1, "inspect the evidence"), (3, "compare evidence")] {
        assert_eq!(replay[index].reasoning_content.as_deref(), Some(reasoning));
        assert_eq!(
            replay[index].reasoning_content,
            next[index].reasoning_content
        );
    }
}

#[test]
fn replay_cuts_where_the_partition_cuts() {
    let mut conversation = Conversation::new();
    conversation.add_system("system prompt".to_string());
    tool_turn(&mut conversation, 0, 500);
    tool_turn(&mut conversation, 1, 10);

    let partition =
        partition_for_compaction(&conversation, &config(180, 1_000), &DefaultTokenCounter, 0)
            .expect("the first tool unit must be collapsed");
    assert_eq!(partition.cut, 4);
    let replay = replay_summary_messages(&conversation, partition.cut);
    assert_eq!(replay.len(), 5);
    assert_eq!(replay[3].tool_call_id.as_deref(), Some("call-0"));
    assert_eq!(replay[4].role, "user");
}

#[test]
fn replay_instruction_names_the_eight_sections() {
    assert_eq!(
        REPLAY_SUMMARY_INSTRUCTION
            .lines()
            .filter_map(|line| line.strip_prefix("## "))
            .collect::<Vec<_>>(),
        [
            "Primary request and intent",
            "Key technical concepts",
            "Files and code",
            "Errors and fixes",
            "Pending work",
            "Current work",
            "Next step",
            "Critical context",
        ],
    );
}

mod replay_compaction {
    use super::*;
    use serde_json::{Value, json};

    const CHECKPOINT: &str = "## Primary request and intent\n- Preserve the deployment decision.\n\
## Key technical concepts\n- Use the deployment notes.\n\
## Files and code\n(none)\n\
## Errors and fixes\n(none)\n\
## Pending work\n- Finish the rollout.\n\
## Current work\n- Read the deployment notes.\n\
## Next step\n- Verify the rollout.\n\
## Critical context\n- MIDDLE_REQUIRED_FACT";
    const FALLBACK: &str = "Hierarchical summary kept MIDDLE_REQUIRED_FACT.";

    fn conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.add_system("session instructions".to_string());
        conversation.add_user("deployment MIDDLE_REQUIRED_FACT ".repeat(500));
        conversation.add_assistant(Some("deployment notes read".to_string()), None, vec![]);
        conversation.add_user("continue rollout".to_string());
        conversation
    }

    fn conversation_with_historical_tool_image() -> Conversation {
        let mut conversation = conversation();
        tool_turn(&mut conversation, 0, 10);
        let Some(Message::Tool { images, .. }) = conversation.messages.last_mut() else {
            panic!("expected tool result");
        };
        images.push(ImageInput {
            source: ImageSource::Url {
                url: "https://example.test/snapshot.png".to_string(),
            },
            detail: ImageDetail::Low,
        });
        conversation.add_user("current request".to_string());
        conversation
    }

    fn server(replay_delta: Value, replay_finish: &'static str) -> SummaryServer {
        SummaryServer::with_reply(200, move |request| {
            let replay = request.model == orca_core::model::PRO_MODEL;
            let (delta, finish_reason) =
                if !request.messages.last().is_some_and(|message| {
                    message.content.as_str() == Some(REPLAY_SUMMARY_INSTRUCTION)
                }) && replay
                {
                    (json!({"content": "ordinary reply"}), "stop")
                } else if replay {
                    (replay_delta.clone(), replay_finish)
                } else {
                    (json!({"content": FALLBACK}), "stop")
                };
            json!({
                "choices": [{"delta": delta, "finish_reason": finish_reason}],
                "usage": {"prompt_tokens": 2500, "completion_tokens": 90, "prompt_cache_hit_tokens": 2048}
            })
        })
    }

    fn compact(
        server: &SummaryServer,
        conversation: &Conversation,
        window: usize,
    ) -> CompactionResult {
        let mut config = config(1_000, 1_500);
        config.max_tokens = window;
        let mut provider = server.config();
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());
        provider.reasoning_effort = ReasoningEffort::High;
        provider.tools_override = Some(
            ["zeta", "alpha"]
                .map(|name| crate::tool_schema::ProviderToolDefinition {
                    name: name.to_string(),
                    description: format!("Read {name}"),
                    input_schema: json!({"type": "object", "properties": {}}),
                    strict_capable: false,
                })
                .to_vec(),
        );
        warm(server, conversation, &provider);
        compact_with_summary(ProviderKind::DeepSeek, conversation, &config, &provider)
    }

    fn assert_fallback(delta: Value, finish_reason: &'static str) {
        let server = server(delta, finish_reason);
        let result = compact(&server, &conversation(), 100_000);
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].model, orca_core::model::PRO_MODEL);
        assert_eq!(requests[1].model, orca_core::model::auxiliary_model());
        assert_eq!(result.observation.summary_requests[0].outcome, "failed");
        assert!(result.observation.summary_requests[0].usage.is_some());
        assert_eq!(result.observation.summary_requests[1].outcome, "succeeded");
    }

    fn reply_to_ordinary_or_replay(request: &SummaryRequest) -> Value {
        let replay = request
            .messages
            .last()
            .is_some_and(|message| message.content.as_str() == Some(REPLAY_SUMMARY_INSTRUCTION));
        let content = if replay { CHECKPOINT } else { "ordinary reply" };
        json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
    }

    fn ordinary_request(conversation: &Conversation, provider: &ProviderConfig) {
        let response = crate::call_streaming(
            ProviderKind::DeepSeek,
            conversation,
            provider,
            &CancelToken::new(),
            &mut |_| {},
        );
        assert!(response.error().is_none(), "{:?}", response.error());
    }

    fn warm(server: &SummaryServer, conversation: &Conversation, provider: &ProviderConfig) {
        ordinary_request(conversation, provider);
        server.requests.lock().unwrap().clear();
    }

    fn wide_config() -> ContextConfig {
        let mut config = config(1_000, 1_500);
        config.max_tokens = 100_000;
        config
    }

    #[test]
    fn replay_never_sends_auto_as_the_model() {
        let server = SummaryServer::with_reply(200, reply_to_ordinary_or_replay);
        let conversation = conversation();
        let mut provider = server.config();
        provider.model = Some(orca_core::model::FLASH_MODEL.to_string());
        ordinary_request(&conversation, &provider);
        provider.model = Some(orca_core::model::AUTO_MODEL.to_string());
        let result = compact_with_summary(
            ProviderKind::DeepSeek,
            &conversation,
            &wide_config(),
            &provider,
        );
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].model, orca_core::model::FLASH_MODEL);
    }

    #[test]
    fn a_pro_replay_sends_image_descriptions_like_the_ordinary_request() {
        let server = SummaryServer::with_reply(200, reply_to_ordinary_or_replay);
        let mut provider = server.config();
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());
        let mut conversation = conversation();
        let Message::User { images, .. } = &mut conversation.messages[1] else {
            panic!("expected user image")
        };
        images.push(ImageInput {
            source: ImageSource::Url {
                url: "https://example.test/input.png".to_string(),
            },
            detail: ImageDetail::Low,
        });
        let description = "[Image analysis] The dashboard shows the rollout completed.";
        conversation
            .messages
            .insert(2, Message::user(description.to_string()));
        conversation.messages[3] = Message::Assistant {
            content: Some("older notes ".repeat(1_000)),
            reasoning_content: None,
            tool_calls: vec![],
            cut_short: None,
            pinned: false,
        };
        let mut prepared = conversation.clone();
        let Message::User { images, .. } = &mut prepared.messages[1] else {
            panic!("expected user image")
        };
        images.clear();
        deepseek_http::bind_replay_source(&mut prepared, &conversation);
        ordinary_request(&prepared, &provider);

        let result = compact_with_summary(
            ProviderKind::DeepSeek,
            &conversation,
            &wide_config(),
            &provider,
        );
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let replay = &requests[1].messages[..requests[1].messages.len() - 1];
        assert_eq!(replay, &requests[0].messages[..replay.len()]);
        assert!(
            replay
                .iter()
                .any(|message| message.content.as_str() == Some(description))
        );
        assert!(
            replay
                .iter()
                .all(|message| message.content.is_string() || message.content.is_null())
        );
    }

    #[test]
    fn replay_uses_plain_tools_after_a_strict_fallback() {
        let server = SummaryServer::with_status_and_reply(|request| {
            let strict = request
                .tools
                .iter()
                .any(|tool| tool["function"]["strict"] == true);
            if strict {
                (
                    400,
                    json!({"error": {"message": "invalid tools: strict schema rejected"}}),
                )
            } else {
                (200, reply_to_ordinary_or_replay(request))
            }
        });
        let conversation = conversation();
        let mut provider = server.config();
        provider.base_url = Some(format!("{}/beta", server.base_url));
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());
        provider.tools_override = Some(vec![crate::tool_schema::ProviderToolDefinition {
            name: "read".to_string(),
            description: "Read notes".to_string(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            strict_capable: true,
        }]);
        ordinary_request(&conversation, &provider);
        let result = compact_with_summary(
            ProviderKind::DeepSeek,
            &conversation,
            &wide_config(),
            &provider,
        );
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].tools[0]["function"]["strict"], true);
        assert_eq!(requests[1].tools, requests[2].tools);
        assert!(requests[2].tools[0]["function"]["strict"].is_null());
    }

    #[test]
    fn replay_matches_the_shared_prefix_in_two_captured_http_requests() {
        let server = SummaryServer::with_reply(200, reply_to_ordinary_or_replay);
        let mut provider = server.config();
        provider.model = Some(orca_core::model::FLASH_MODEL.to_string());
        let mut conversation = conversation_with_historical_tool_image();
        conversation.replace_plan_state("plan overlay".to_string());
        conversation.mcp_instructions =
            vec![("notes".to_string(), "Use search before fetch.".to_string())];
        let config = wide_config();
        let image = conversation
            .messages
            .iter()
            .find_map(|message| message.images().first())
            .unwrap()
            .clone();
        let add_screenshot = |conversation: &mut Conversation, id: usize| {
            conversation.add_assistant(
                None,
                Some("inspect capture".to_string()),
                vec![RawToolCall {
                    id: format!("snapshot-{id}"),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            );
            conversation.add_tool_result(format!("snapshot-{id}"), "captured screen".to_string());
            let Some(Message::Tool { images, .. }) = conversation.messages.last_mut() else {
                panic!("expected screenshot")
            };
            images.push(image.clone());
        };
        add_screenshot(&mut conversation, 1);
        add_screenshot(&mut conversation, 2);
        ordinary_request(&conversation, &provider);
        add_screenshot(&mut conversation, 3);

        let result =
            compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider);
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let ordinary = &requests[0].messages;
        assert_eq!(ordinary[ordinary.len() - 2].text(), "plan overlay");
        let guidance = ordinary.last().unwrap();
        assert_eq!(guidance.role, "system");
        assert!(guidance.text().contains(
            "<mcp_server_instructions server=\"notes\">\nUse search before fetch.\n</mcp_server_instructions>"
        ));
        let replay = &requests[1].messages[..requests[1].messages.len() - 1];
        assert!(replay.len() <= ordinary.len() - 2);
        assert!(
            requests[1].wire_messages[..replay.len()]
                .iter()
                .zip(&requests[0].wire_messages)
                .all(|(replay, ordinary)| replay.get() == ordinary.get())
        );
        assert!(
            replay
                .iter()
                .any(|message| message.role == "tool" && message.content.is_array())
        );
        assert_eq!(
            result.conversation.mcp_instructions,
            conversation.mcp_instructions
        );
    }

    #[test]
    fn replay_requires_a_successful_matching_request_covering_the_cut() {
        for change in [
            "unwarmed",
            "edited history",
            "new model",
            "cut beyond capture",
        ] {
            let server = server(json!({"content": CHECKPOINT}), "stop");
            let mut provider = server.config();
            provider.model = Some(orca_core::model::PRO_MODEL.to_string());
            let mut conversation = conversation();
            if change != "unwarmed" {
                warm(&server, &conversation, &provider);
            }
            match change {
                "edited history" => {
                    conversation.messages[1] = Message::user("changed evidence ".repeat(1_000))
                }
                "new model" => provider.model = Some(orca_core::model::FLASH_MODEL.to_string()),
                "cut beyond capture" => {
                    conversation.add_assistant(Some("new evidence ".repeat(1_000)), None, vec![]);
                    conversation.add_user("latest request".to_string());
                }
                _ => {}
            }
            let result = compact_with_summary(
                ProviderKind::DeepSeek,
                &conversation,
                &wide_config(),
                &provider,
            );
            assert!(
                matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK),
                "{change}"
            );
            assert!(
                result
                    .observation
                    .summary_requests
                    .iter()
                    .all(|request| request.purpose != REPLAY_SUMMARY_PURPOSE),
                "{change}"
            );
        }
    }

    #[test]
    fn deep_compaction_uses_the_replay_checkpoint() {
        let server = server(json!({"content": CHECKPOINT}), "stop");
        let mut conversation = conversation();
        conversation.repeat_tool_streak = Some(orca_core::conversation::RepeatToolStreak {
            tool_name: "read_file".into(),
            arguments: "{}".into(),
            count: 2,
        });
        let result = compact(&server, &conversation, 100_000);
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT)
        );
        assert_eq!(
            result.conversation.summary.baseline.as_deref(),
            Some(CHECKPOINT)
        );
        assert_eq!(
            result.conversation.repeat_tool_streak,
            conversation.repeat_tool_streak
        );
        let requests = server.requests.lock().unwrap();
        let [request] = requests.as_slice() else {
            panic!("expected one checkpoint request, got {}", requests.len())
        };
        assert_eq!(request.model, orca_core::model::PRO_MODEL);
        assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(request.thinking.kind, "enabled");
        assert_eq!(request.max_tokens, 32_768);
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(
            request.messages.last().unwrap().text(),
            REPLAY_SUMMARY_INSTRUCTION
        );
    }

    #[test]
    fn replay_checkpoint_keeps_existing_and_new_spill_references() {
        use orca_core::tool_types::{ToolOutputSpill, ToolStatus, ToolTerminal};

        let mut conversation = conversation();
        let current_request = conversation.messages.pop().unwrap();
        let dir = std::env::current_dir().unwrap().join("spill-fixture");
        let earlier = ToolOutputSpill::new(dir.clone(), "earlier".into(), 10_000).unwrap();
        let collapsed = ToolOutputSpill::new(dir, "call-0".into(), 10_000).unwrap();
        conversation
            .summary
            .tool_output_spills
            .push(earlier.clone());
        tool_turn(&mut conversation, 0, 1_000);
        let Some(Message::Tool { terminal, .. }) = conversation.messages.last_mut() else {
            panic!("expected tool result");
        };
        let mut receipt = ToolTerminal::try_from_parts(
            ToolStatus::Completed,
            None,
            None,
            true,
            None,
            Default::default(),
            Default::default(),
        )
        .unwrap();
        receipt.spill = Some(Box::new(collapsed.clone()));
        *terminal = Some(receipt);
        conversation.messages.push(current_request);

        let server = server(json!({"content": CHECKPOINT}), "stop");
        let result = compact(&server, &conversation, 100_000);
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == CHECKPOINT),
            "{:?}",
            result.observation.summary_requests
        );
        let requests = server.requests.lock().unwrap();
        let [request] = requests.as_slice() else {
            panic!("expected one checkpoint request, got {}", requests.len());
        };
        assert_eq!(request.messages[request.messages.len() - 2].role, "tool");
        assert_eq!(
            result.conversation.summary.tool_output_spills,
            [earlier, collapsed]
        );
    }

    #[test]
    fn a_replay_cost_receipt_stops_the_remote_chain_before_fallback() {
        let server = server(json!({"content": "incomplete checkpoint"}), "stop");
        let mut provider = server.config();
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());
        let conversation = conversation();
        warm(&server, &conversation, &provider);
        let mut receipt = None;
        let result = compact_with_summary_settled(
            ProviderKind::DeepSeek,
            &conversation,
            &wide_config(),
            &provider,
            None,
            false,
            &mut |model, usage| {
                if let Some(usage) = usage {
                    receipt = Some((model.to_string(), usage));
                    return false;
                }
                true
            },
        );
        assert_eq!(
            receipt,
            Some((
                orca_core::model::PRO_MODEL.to_string(),
                Usage {
                    input_tokens: 2500,
                    output_tokens: 90,
                    cache_tokens: 2048,
                }
            ))
        );
        assert!(matches!(result.kind, CompactionKind::LocalTruncation));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert_eq!(result.observation.summary_requests.len(), 1);
        assert_eq!(
            result.observation.summary_requests[0].outcome,
            "budget_stopped"
        );
    }

    #[test]
    fn replay_admission_uses_provider_calibration_and_the_request_margin() {
        for (words, calibrated) in [(55_000, true), (60_000, false)] {
            let server = server(json!({"content": CHECKPOINT}), "stop");
            let mut conversation = Conversation::new();
            conversation.add_system("instructions".to_string());
            conversation.add_user("evidence ".repeat(words));
            conversation.add_user("current request".to_string());
            let mut provider = server.config();
            provider.model = Some(orca_core::model::PRO_MODEL.to_string());
            warm(&server, &conversation, &provider);
            if calibrated {
                let estimated = wire_equivalent_tokens(&conversation, &provider);
                conversation.record_usage_anchor(
                    (estimated * 3 / 2) as u64,
                    estimated,
                    conversation.messages.len(),
                );
            }
            let config = config(30_000, 100_000);
            let result =
                compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider);
            assert!(
                matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
            );
            assert!(
                result
                    .observation
                    .summary_requests
                    .iter()
                    .all(|request| request.purpose != REPLAY_SUMMARY_PURPOSE)
            );
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.model == orca_core::model::FLASH_MODEL)
            );
        }
    }

    #[test]
    fn replay_is_skipped_when_it_would_not_fit() {
        let server = server(json!({"content": CHECKPOINT}), "stop");
        let result = compact(&server, &conversation(), 33_000);
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, orca_core::model::auxiliary_model());
    }

    #[test]
    fn a_checkpoint_without_the_eight_headings_falls_back() {
        for checkpoint in [
            CHECKPOINT.replace("## Next step\n", ""),
            CHECKPOINT
                .replace("## Next step", "## placeholder")
                .replace("## Critical context", "## Next step")
                .replace("## placeholder", "## Critical context"),
        ] {
            assert_fallback(json!({"content": checkpoint}), "stop");
        }
    }

    #[test]
    fn a_reply_with_tool_calls_falls_back() {
        assert_fallback(
            json!({"content": CHECKPOINT, "tool_calls": [{
                "index": 0, "id": "call-1", "type": "function",
                "function": {"name": "alpha", "arguments": "{}"}
            }]}),
            "tool_calls",
        );
    }

    #[test]
    fn an_empty_or_reasoning_only_reply_falls_back() {
        for delta in [json!({}), json!({"reasoning_content": "Planning only"})] {
            assert_fallback(delta, "stop");
        }
    }

    #[test]
    fn a_checkpoint_not_smaller_than_the_range_falls_back() {
        assert_fallback(
            json!({"content": format!("{CHECKPOINT}\n{}", "checkpoint detail ".repeat(3500))}),
            "stop",
        );
    }

    #[test]
    fn a_replay_checkpoint_that_fails_the_gain_rule_falls_back_to_the_hierarchy() {
        let checkpoint = format!("{CHECKPOINT}\n{}", "fact ".repeat(15_000));
        let server = server(json!({"content": checkpoint}), "stop");
        let mut conversation = Conversation::new();
        conversation.add_system("instructions".to_string());
        conversation.add_user("evidence ".repeat(20_000));
        conversation.add_system_pinned("keep this notice".to_string());
        let current = "current ".repeat(138_000);
        conversation.add_user(current.clone());
        let config = config(140_000, 256_000);
        let mut provider = server.config();
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());

        warm(&server, &conversation, &provider);
        let result =
            compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider);

        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
        );
        assert!(measure_prompt(&result.conversation, &provider).tokens <= config.soft_limit());
        assert_eq!(
            result.conversation.last_user_message(),
            Some(current.as_str())
        );
        assert!(result.conversation.messages.iter().any(|message| {
            message.is_pinned() && message.content_str() == Some("keep this notice")
        }));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0].model, orca_core::model::PRO_MODEL);
        assert_eq!(result.observation.summary_requests[0].outcome, "failed");
        assert!(requests.len() > 1);
        assert!(
            requests[1..]
                .iter()
                .all(|request| request.model == orca_core::model::FLASH_MODEL)
        );
    }

    #[test]
    fn a_replay_checkpoint_replaces_the_previous_summary() {
        let server = server(json!({"content": CHECKPOINT}), "stop");
        let mut conversation = conversation();
        conversation.summary.baseline = Some("prior baseline".to_string());
        conversation.summary.deltas = vec!["prior delta".to_string()];
        let result = compact(&server, &conversation, 100_000);
        assert_eq!(
            result.conversation.summary.baseline.as_deref(),
            Some(CHECKPOINT)
        );
        assert!(result.conversation.summary.deltas.is_empty());
        assert_eq!(
            result.conversation.rolling_summary.as_deref(),
            Some(CHECKPOINT)
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].messages[1].text(),
            format!(
                "{}[Summary baseline]\nprior baseline",
                deepseek_http::SUMMARY_PREAMBLE
            )
        );
        assert_eq!(
            requests[0].messages[2].text(),
            "[Summary update 1]\nprior delta"
        );
    }

    #[test]
    fn replay_usage_is_recorded_in_the_observation() {
        let server = server(json!({"content": CHECKPOINT}), "stop");
        let result = compact(&server, &conversation(), 100_000);
        let [request] = result.observation.summary_requests.as_slice() else {
            panic!("expected a single observation")
        };
        assert_eq!(request.purpose, "replay");
        assert_eq!(request.model, orca_core::model::PRO_MODEL);
        assert_eq!(
            request.usage,
            Some(Usage {
                input_tokens: 2500,
                output_tokens: 90,
                cache_tokens: 2048
            })
        );
        assert_eq!(request.outcome, "succeeded");
        assert!(!request.summary_cache_hit);
    }

    #[test]
    fn a_checkpoint_that_leaves_no_room_for_a_reply_falls_back() {
        let checkpoint = format!("{CHECKPOINT}\n{}", "fact ".repeat(5_000));
        let server = server(json!({"content": checkpoint}), "stop");
        let mut conversation = Conversation::new();
        conversation.add_system("instructions".to_string());
        conversation.add_user("evidence ".repeat(8_000));
        conversation.add_user("current ".repeat(22_000));
        let mut config = config(27_000, 60_000);
        config.reserved_for_response = 16_384;
        let mut provider = server.config();
        provider.model = Some(orca_core::model::PRO_MODEL.to_string());
        let estimated = wire_equivalent_tokens(&conversation, &provider);
        conversation.record_usage_anchor(
            (estimated * 3 / 2) as u64,
            estimated,
            conversation.messages.len(),
        );

        warm(&server, &conversation, &provider);
        let result =
            compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider);
        let prompt_tokens = wire_equivalent_tokens(&result.conversation, &provider) * 3 / 2;
        assert!(
            config.request_reply_budget(prompt_tokens).is_some(),
            "compacted prompt of {prompt_tokens} tokens left no room for a reply"
        );
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
        );
        assert_eq!(result.observation.summary_requests[0].outcome, "failed");
        assert!(result.observation.summary_requests[0].usage.is_some());
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0].model, orca_core::model::PRO_MODEL);
        assert!(requests.len() > 1);
        assert!(
            requests[1..]
                .iter()
                .all(|request| request.model == orca_core::model::FLASH_MODEL)
        );
    }

    #[test]
    fn replay_falls_back_when_the_model_cannot_receive_historical_tool_images() {
        let server = server(json!({"content": CHECKPOINT}), "stop");
        let result = compact(&server, &conversation_with_historical_tool_image(), 100_000);
        let requests = server.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|request| request.model == orca_core::model::FLASH_MODEL)
        );
        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(summary) if summary == FALLBACK)
        );
    }

    #[test]
    fn replay_preserves_tool_images_on_a_model_that_accepts_them() {
        let server = SummaryServer::with_reply(200, |_| {
            json!({
                "choices": [{"delta": {"content": CHECKPOINT}, "finish_reason": "stop"}]
            })
        });
        let conversation = conversation_with_historical_tool_image();
        let mut config = config(1_000, 1_500);
        config.max_tokens = 100_000;
        let mut provider = server.config();
        provider.model = Some(orca_core::model::FLASH_MODEL.to_string());

        warm(&server, &conversation, &provider);
        let result =
            compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider);
        assert!(matches!(result.kind, CompactionKind::RemoteSummary(_)));
        let requests = server.requests.lock().unwrap();
        let [request] = requests.as_slice() else {
            panic!("expected one checkpoint request, got {}", requests.len());
        };
        assert_eq!(
            request.messages.last().unwrap().text(),
            REPLAY_SUMMARY_INSTRUCTION
        );
        assert!(
            request
                .messages
                .iter()
                .any(|message| { message.role == "assistant" && message.content.is_null() })
        );
        assert!(request.messages.iter().any(|message| {
            message.role == "tool"
                && message.content[1]["image_url"]["url"] == "https://example.test/snapshot.png"
        }));
    }
}
