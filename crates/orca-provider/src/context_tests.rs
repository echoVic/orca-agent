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
            *content = micro_compact_tool_output(content);
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
fn below_pressure_keeps_exact_prefix_and_dynamic_overlay_follows_summary() {
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
    assert_eq!(metrics.unchanged_prefix_messages, 3);
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
fn hard_pressure_keeps_pinned_and_current_tool_turn_atomically() {
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
    let normalized = normalized_for_compaction(&conversation);
    let active = turn_ranges(&normalized.messages).last().unwrap().clone();
    let expected = format_messages(&normalized.messages[active]);
    let config = config(500, 700);
    let result = compact_with_summary(ProviderKind::DeepSeek, &conversation, &config, &provider());
    let active = turn_ranges(&result.conversation.messages)
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        format_messages(&result.conversation.messages[active]),
        expected
    );
    assert!(result.conversation.messages.iter().any(|message| matches!(message,
        Message::Assistant { tool_calls, .. } if tool_calls.iter().any(|call| call.id == "call-0"))));
    assert!(
        result
            .conversation
            .messages
            .iter()
            .any(|message| matches!(message,
        Message::Tool { tool_call_id, terminal: Some(_), .. } if tool_call_id == "pending-a"))
    );
    assert!(
        !result
            .conversation
            .messages
            .iter()
            .any(|message| message.content_str() == Some("inspect file 2"))
    );
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
    let reduced = cache_aware_micro_compaction(&conversation, 0, 1_536, conversation_tokens);
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
}

#[derive(Debug, Deserialize)]
struct SummaryMessage {
    role: String,
    content: String,
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
                        let request: SummaryRequest = serde_json::from_str(&body).unwrap();
                        captured.lock().unwrap().push(request);
                        let body = format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            serde_json::json!({
                                "choices": [{"delta": {"content": "Preserve MIDDLE_REQUIRED_FACT and pending work."}, "finish_reason": finish_reason}],
                                "usage": {"prompt_tokens": 100, "completion_tokens": 12, "prompt_cache_hit_tokens": 64}
                            })
                        );
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
            bytes.len() <= 256 * 1024,
            "summary fixture request exceeds bound"
        );
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            assert!(headers.starts_with("POST /chat/completions "));
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
                && DefaultTokenCounter.count_text(&request.messages[1].content)
                    <= SUMMARY_INPUT_TOKENS + 100)
    );
    assert!(
        requests
            .iter()
            .any(|request| request.messages[1].content.contains("MIDDLE_REQUIRED_FACT"))
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
