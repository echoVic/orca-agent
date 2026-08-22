use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde::Deserialize;

use orca_core::cancel::CancelToken;
use orca_core::provider_types::{ToolCallProgress, Usage};

const TOOL_CALL_PROGRESS_ARGUMENT_BYTES_STEP: usize = 8 * 1024;
const INVALID_SSE_DATA_JSON_PREFIX: &str = "invalid SSE data JSON";
const INVALID_SSE_UTF8_PREFIX: &str = "invalid UTF-8 in SSE data";
const PREMATURE_STREAM_EOF_ERROR: &str = "stream ended before terminal marker";

#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    pub usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkDelta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(alias = "reasoning")]
    pub reasoning_alias: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

impl ChunkDelta {
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning_alias.as_deref())
    }
}

#[derive(Debug, Deserialize)]
pub struct StreamToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct StreamFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Debug)]
pub struct StreamResult {
    pub finish_reason: Option<String>,
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCallAccumulator>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct StreamUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

impl From<StreamUsage> for Usage {
    fn from(usage: StreamUsage) -> Self {
        let input_tokens = usage.prompt_tokens.unwrap_or_else(|| {
            usage.prompt_cache_hit_tokens.unwrap_or(0) + usage.prompt_cache_miss_tokens.unwrap_or(0)
        });
        let output_tokens = usage.completion_tokens.unwrap_or_else(|| {
            usage
                .total_tokens
                .unwrap_or(input_tokens)
                .saturating_sub(input_tokens)
        });
        Self {
            input_tokens,
            output_tokens,
            cache_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
        }
    }
}

pub enum StreamEvent<'a> {
    Reasoning(&'a str),
    Content(&'a str),
    ToolCallProgress(ToolCallProgress),
}

pub fn parse_sse_stream<R: Read>(
    reader: R,
    cancel: &CancelToken,
    mut on_delta: impl FnMut(StreamEvent),
) -> Result<StreamResult, String> {
    let buf_reader = BufReader::new(reader);
    let mut accumulator = StreamAccumulator::default();

    for line in buf_reader.lines() {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let line = line.map_err(|e| {
            if cancel.is_cancelled() {
                "cancelled".to_string()
            } else {
                format!("stream read error: {e}")
            }
        })?;
        if accumulator.push_line(&line, cancel, &mut on_delta)? {
            break;
        }
    }

    if cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    accumulator.finish()
}

pub(crate) async fn parse_sse_response(
    mut response: reqwest::Response,
    cancel: &CancelToken,
    idle_timeout: Duration,
    mut on_delta: impl FnMut(StreamEvent),
) -> Result<StreamResult, String> {
    let mut accumulator = StreamAccumulator::default();
    let mut buffer = Vec::new();

    loop {
        let next = tokio::select! {
            biased;
            _ = crate::http_client::wait_for_cancel(cancel) => {
                return Err("cancelled".to_string());
            }
            result = tokio::time::timeout(idle_timeout, response.chunk()) => result,
        };

        let chunk = match next {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                if !buffer.is_empty() {
                    buffer.push(b'\n');
                    let _ = process_complete_lines(
                        &mut buffer,
                        cancel,
                        &mut accumulator,
                        &mut on_delta,
                    )?;
                }
                return accumulator.finish();
            }
            Ok(Err(error)) => {
                if cancel.is_cancelled() {
                    return Err("cancelled".to_string());
                }
                return Err(format!("stream read error: {error}"));
            }
            Err(_) => {
                return Err(format!(
                    "stream read error: idle read timed out after {idle_timeout:?}"
                ));
            }
        };

        buffer.extend_from_slice(&chunk);
        if process_complete_lines(&mut buffer, cancel, &mut accumulator, &mut on_delta)? {
            return accumulator.finish();
        }
    }
}

fn process_complete_lines(
    buffer: &mut Vec<u8>,
    cancel: &CancelToken,
    accumulator: &mut StreamAccumulator,
    on_delta: &mut impl FnMut(StreamEvent),
) -> Result<bool, String> {
    let mut consumed = 0;
    let mut done = false;
    while let Some(relative_newline) = buffer[consumed..].iter().position(|byte| *byte == b'\n') {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let newline = consumed + relative_newline;
        let mut line = &buffer[consumed..newline];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        let line = std::str::from_utf8(line)
            .map_err(|error| format!("{INVALID_SSE_UTF8_PREFIX}: {error}"))?;
        if accumulator.push_line(line, cancel, on_delta)? {
            done = true;
            consumed = newline + 1;
            break;
        }
        consumed = newline + 1;
    }
    if consumed > 0 {
        buffer.drain(..consumed);
    }
    Ok(done)
}

#[derive(Default)]
struct StreamAccumulator {
    saw_done: bool,
    finish_reason: Option<String>,
    reasoning: String,
    content: String,
    tool_calls: Vec<ToolCallAccumulator>,
    tool_call_progress: ToolCallProgressTracker,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    fn push_line(
        &mut self,
        line: &str,
        cancel: &CancelToken,
        on_delta: &mut impl FnMut(StreamEvent),
    ) -> Result<bool, String> {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with(':') {
            return Ok(false);
        }

        let Some(data) = line.strip_prefix("data: ") else {
            return Ok(false);
        };
        if data == "[DONE]" {
            self.saw_done = true;
            return Ok(true);
        }

        let chunk: StreamChunk = serde_json::from_str(data)
            .map_err(|error| format!("{INVALID_SSE_DATA_JSON_PREFIX}: {error}"))?;

        if let Some(chunk_usage) = chunk.usage {
            self.usage = Some(chunk_usage.into());
        }

        for choice in &chunk.choices {
            if let Some(ref reason) = choice.finish_reason {
                self.finish_reason = Some(reason.clone());
            }

            let delta = &choice.delta;

            if let Some(text) = delta.reasoning()
                && !text.is_empty()
            {
                self.reasoning.push_str(text);
                if cancel.is_cancelled() {
                    return Err("cancelled".to_string());
                }
                on_delta(StreamEvent::Reasoning(text));
            }

            if let Some(ref text) = delta.content
                && !text.is_empty()
            {
                if cancel.is_cancelled() {
                    return Err("cancelled".to_string());
                }
                self.content.push_str(text);
                on_delta(StreamEvent::Content(text));
            }

            if let Some(ref tcs) = delta.tool_calls {
                for tc_delta in tcs {
                    accumulate_tool_call(&mut self.tool_calls, tc_delta);
                    if let Some(progress) = self
                        .tool_call_progress
                        .progress_for_delta(&self.tool_calls, tc_delta.index)
                    {
                        if cancel.is_cancelled() {
                            return Err("cancelled".to_string());
                        }
                        on_delta(StreamEvent::ToolCallProgress(progress));
                    }
                }
            }
        }

        Ok(false)
    }

    fn finish(self) -> Result<StreamResult, String> {
        if !self.saw_done && self.finish_reason.is_none() {
            return Err(PREMATURE_STREAM_EOF_ERROR.to_string());
        }
        Ok(StreamResult {
            finish_reason: self.finish_reason,
            reasoning: self.reasoning,
            content: self.content,
            tool_calls: self.tool_calls,
            usage: self.usage,
        })
    }
}

pub(crate) fn is_stream_integrity_error(error: &str) -> bool {
    error == PREMATURE_STREAM_EOF_ERROR
        || error.starts_with(INVALID_SSE_DATA_JSON_PREFIX)
        || error.starts_with(INVALID_SSE_UTF8_PREFIX)
}

fn accumulate_tool_call(buf: &mut Vec<ToolCallAccumulator>, delta: &StreamToolCallDelta) {
    let idx = delta.index;
    while buf.len() <= idx {
        buf.push(ToolCallAccumulator {
            id: String::new(),
            function_name: String::new(),
            arguments: String::new(),
        });
    }
    if let Some(ref id) = delta.id {
        buf[idx].id.clone_from(id);
    }
    if let Some(ref func) = delta.function {
        if let Some(ref name) = func.name {
            buf[idx].function_name.push_str(name);
        }
        if let Some(ref args) = func.arguments {
            buf[idx].arguments.push_str(args);
        }
    }
}

#[derive(Default)]
struct ToolCallProgressTracker {
    calls: Vec<ToolCallProgressState>,
}

#[derive(Default)]
struct ToolCallProgressState {
    last_emitted_arguments_bytes: Option<usize>,
}

impl ToolCallProgressTracker {
    fn progress_for_delta(
        &mut self,
        buf: &[ToolCallAccumulator],
        index: usize,
    ) -> Option<ToolCallProgress> {
        while self.calls.len() <= index {
            self.calls.push(ToolCallProgressState::default());
        }
        let current = buf.get(index)?;
        if current.id.is_empty() || current.function_name.is_empty() {
            return None;
        }
        let arguments_bytes = current.arguments.len();
        let state = self.calls.get_mut(index)?;
        let should_emit = match state.last_emitted_arguments_bytes {
            None => true,
            Some(last) => {
                arguments_bytes.saturating_sub(last) >= TOOL_CALL_PROGRESS_ARGUMENT_BYTES_STEP
            }
        };
        if !should_emit {
            return None;
        }
        state.last_emitted_arguments_bytes = Some(arguments_bytes);
        Some(tool_call_progress(current, arguments_bytes))
    }
}

fn tool_call_progress(current: &ToolCallAccumulator, arguments_bytes: usize) -> ToolCallProgress {
    ToolCallProgress {
        id: current.id.clone(),
        function_name: Some(current.function_name.clone()),
        arguments_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn parse_simple_content_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut content_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::Content(text) = delta {
                content_parts.push(text.to_string());
            }
        })
        .unwrap();

        assert_eq!(result.content, "Hello world");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(content_parts, vec!["Hello", " world"]);
    }

    #[test]
    fn parse_reasoning_and_content() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut reasoning_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::Reasoning(text) = delta {
                reasoning_parts.push(text.to_string());
            }
        })
        .unwrap();

        assert_eq!(result.reasoning, "thinking...");
        assert_eq!(result.content, "answer");
        assert_eq!(reasoning_parts, vec!["thinking..."]);
    }

    #[test]
    fn parse_tool_calls_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":null,\"arguments\":\"{\\\"path\\\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":null,\"arguments\":\": \\\"src/main.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {}).unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].function_name, "read_file");
        assert_eq!(
            result.tool_calls[0].arguments,
            "{\"path\": \"src/main.rs\"}"
        );
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn parse_tool_calls_stream_rejects_malformed_data_frame() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"partial\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let error = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {})
            .expect_err("malformed SSE data must not be discarded");

        assert!(error.contains("invalid SSE data JSON"), "{error}");
    }

    #[test]
    fn parse_tool_calls_stream_rejects_premature_eof() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"partial\"}}]},\"finish_reason\":null}]}\n\n";

        let cancel = CancelToken::new();
        let error = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {})
            .expect_err("EOF without a finish reason or [DONE] must fail");

        assert_eq!(error, "stream ended before terminal marker");
    }

    #[test]
    fn invalid_utf8_stream_error_is_retryable_before_visible_output() {
        assert!(is_stream_integrity_error(
            "invalid UTF-8 in SSE data: invalid utf-8 sequence"
        ));
    }

    #[test]
    fn parse_tool_calls_stream_emits_argument_progress() {
        let large_chunk = "a".repeat(8 * 1024);
        let sse_data = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"function\":{{\"name\":\"write_file\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":null,\"function\":{{\"name\":null,\"arguments\":\"abc\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":null,\"function\":{{\"name\":null,\"arguments\":\"{large_chunk}\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
             data: [DONE]\n\n"
        );

        let cancel = CancelToken::new();
        let mut progress = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::ToolCallProgress(update) = delta {
                progress.push((
                    update.id.clone(),
                    update.function_name.clone(),
                    update.arguments_bytes,
                ));
            }
        })
        .unwrap();

        assert_eq!(result.tool_calls[0].function_name, "write_file");
        assert_eq!(
            progress,
            vec![
                ("call_1".to_string(), Some("write_file".to_string()), 0),
                (
                    "call_1".to_string(),
                    Some("write_file".to_string()),
                    result.tool_calls[0].arguments.len()
                )
            ]
        );
        assert_eq!(result.tool_calls[0].arguments.len(), 3 + large_chunk.len());
    }

    #[test]
    fn parse_tool_calls_stream_waits_for_stable_id_before_progress() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":\"write_file\",\"arguments\":\"abc\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":null,\"arguments\":\"def\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut progress = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::ToolCallProgress(update) = delta {
                progress.push((update.id.clone(), update.arguments_bytes));
            }
        })
        .unwrap();

        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].arguments, "abcdef");
        assert_eq!(progress, vec![("call_1".to_string(), 6)]);
    }

    #[test]
    fn cancel_interrupts_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        cancel.cancel();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {});
        assert_eq!(result.unwrap_err(), "cancelled");
    }

    #[test]
    fn parse_usage_chunk() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n\
                        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30,\"total_tokens\":150,\"prompt_cache_hit_tokens\":10}}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {}).unwrap();

        let usage = result.usage.expect("usage");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_tokens, 10);
    }

    #[test]
    fn incremental_line_buffer_reassembles_utf8_sse_across_chunks() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let split = stream.find('你').expect("multibyte content") + 1;
        let cancel = CancelToken::new();
        let mut buffer = stream.as_bytes()[..split].to_vec();
        let mut accumulator = StreamAccumulator::default();
        let mut content = Vec::new();

        assert!(
            !process_complete_lines(&mut buffer, &cancel, &mut accumulator, &mut |event| {
                if let StreamEvent::Content(text) = event {
                    content.push(text.to_string());
                }
            },)
            .expect("partial chunk")
        );
        buffer.extend_from_slice(&stream.as_bytes()[split..]);
        assert!(
            process_complete_lines(&mut buffer, &cancel, &mut accumulator, &mut |event| {
                if let StreamEvent::Content(text) = event {
                    content.push(text.to_string());
                }
            },)
            .expect("complete chunks")
        );

        let result = accumulator.finish().expect("complete stream");
        assert_eq!(result.content, "你好");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(content, vec!["你好"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_response_parser_times_out_a_stalled_body_read() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled SSE endpoint");
        let address = listener.local_addr().expect("stalled SSE address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled SSE request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read stalled SSE request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .expect("write stalled SSE headers");
            stream.flush().expect("flush stalled SSE headers");
            std::thread::sleep(Duration::from_millis(100));
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/stream"))
            .send()
            .await
            .expect("open stalled SSE response");
        let cancel = CancelToken::new();

        let error = parse_sse_response(response, &cancel, Duration::from_millis(20), |_| {})
            .await
            .expect_err("stalled body must time out");

        server.join().expect("stalled SSE server");
        assert_eq!(error, "stream read error: idle read timed out after 20ms");
    }
}
