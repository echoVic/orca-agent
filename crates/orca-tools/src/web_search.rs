use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

use crate::deepseek_web::{self, DeepSeekSearchHit, DeepSeekWebAccess, DeepSeekWebError};

/// How recent the results must be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Recency {
    /// Published in the last N days, 1 to [`MAX_FRESH_DAYS`].
    LastDays(u32),
    /// Published between two ISO dates.
    Range { start: String, end: String },
}

/// The longest recency window `fresh_days` accepts; larger values are clamped.
const MAX_FRESH_DAYS: u32 = 365;

/// The arguments as the model sends them.
#[derive(Debug, Deserialize)]
struct RawSearchArgs {
    query: String,
    #[serde(default = "default_count")]
    count: usize,
    /// The recency window in days, the parameter the schema advertises.
    #[serde(default, deserialize_with = "deserialize_fresh_days")]
    fresh_days: Option<u32>,
    /// Brave-style codes the schema advertised before `fresh_days`; still
    /// accepted from models and transcripts that use them.
    #[serde(default, deserialize_with = "deserialize_legacy_freshness")]
    freshness: Option<Recency>,
}

#[derive(Debug)]
struct SearchArgs {
    query: String,
    count: usize,
    recency: Option<Recency>,
    /// The recency was guessed from the query's wording, not sent by the
    /// model. DeepSeek turns recency into a hard filter, so it gets only the
    /// recency the model asked for.
    recency_inferred: bool,
}

struct SearchResult {
    title: String,
    url: String,
    description: String,
    /// The publication day as `YYYY-MM-DD`; only DeepSeek reports one.
    published: Option<String>,
}

const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const EXA_ENDPOINT: &str = "https://mcp.exa.ai/mcp";
/// DeepSeek rejects longer queries; the limit counts the operators too.
const DEEPSEEK_QUERY_CHARS: usize = 128;
/// DeepSeek summaries run to several thousand characters; this keeps five
/// results to a few thousand tokens.
const DEEPSEEK_SUMMARY_CHARS: usize = 800;

enum SearchError {
    Cancelled,
    Failed(String),
}

/// Which search service answers a query.
enum SearchBackend<'a> {
    /// Brave, with the key from `BRAVE_SEARCH_API_KEY`.
    Brave(&'a str),
    /// DeepSeek's Search API, falling back to Exa when it fails.
    DeepSeek(&'a DeepSeekWebAccess),
    /// Exa's keyless MCP endpoint.
    Exa,
}

/// A Brave key the user set wins; otherwise DeepSeek when the session may use
/// it, otherwise Exa.
fn choose_search_backend<'a>(
    brave_key: Option<&'a str>,
    deepseek: Option<&'a DeepSeekWebAccess>,
) -> SearchBackend<'a> {
    match (brave_key.filter(|key| !key.trim().is_empty()), deepseek) {
        (Some(key), _) => SearchBackend::Brave(key),
        (None, Some(access)) => SearchBackend::DeepSeek(access),
        (None, None) => SearchBackend::Exa,
    }
}

pub fn execute(request: &ToolRequest, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, max_bytes, None, || false)
}

pub fn execute_or_cancel(
    request: &ToolRequest,
    max_bytes: usize,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if should_cancel() {
        return ToolResult::cancelled_before_start(request, "web search was cancelled");
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to start web search runtime: {error}"),
                None,
            );
        }
    };

    let brave_key = std::env::var("BRAVE_SEARCH_API_KEY").ok();
    let backend = choose_search_backend(brave_key.as_deref(), deepseek);
    let output = runtime.block_on(search_output(
        &args,
        backend,
        EXA_ENDPOINT,
        chrono::Utc::now().date_naive(),
        &should_cancel,
    ));

    match output {
        Ok(output) => {
            let (output, truncated) = truncate_output(output, max_bytes);
            ToolResult::completed(request, output, truncated)
        }
        Err(SearchError::Cancelled) => {
            ToolResult::cancelled(request, "web search was cancelled", None)
        }
        Err(SearchError::Failed(error)) => ToolResult::failed(request, error, None),
    }
}

/// Runs the search on `backend` and renders the results. A failed DeepSeek
/// search is answered by Exa, with a note saying so.
async fn search_output(
    args: &SearchArgs,
    backend: SearchBackend<'_>,
    exa_endpoint: &str,
    today: NaiveDate,
    should_cancel: &dyn Fn() -> bool,
) -> Result<String, SearchError> {
    match backend {
        SearchBackend::Brave(key) => search_brave_or_cancel(args, key, today, should_cancel)
            .await
            .map(format_results),
        SearchBackend::Exa => search_exa_at_or_cancel_async(args, exa_endpoint, should_cancel)
            .await
            .map(format_results),
        SearchBackend::DeepSeek(access) => {
            let recency = (!args.recency_inferred)
                .then_some(args.recency.as_ref())
                .flatten();
            let query = deepseek_query(&args.query, recency, today);
            match deepseek_web::search(access, &query, args.count, should_cancel).await {
                Ok(hits) => Ok(format_results(
                    hits.into_iter().map(deepseek_result).collect(),
                )),
                Err(DeepSeekWebError::Cancelled) => Err(SearchError::Cancelled),
                Err(DeepSeekWebError::Failed(reason)) => {
                    match search_exa_at_or_cancel_async(args, exa_endpoint, should_cancel).await {
                        Ok(results) => Ok(format!(
                            "[DeepSeek search failed: {reason}. These results come from Exa.]\n\n{}",
                            format_results(results)
                        )),
                        Err(SearchError::Cancelled) => Err(SearchError::Cancelled),
                        Err(SearchError::Failed(exa_error)) => Err(SearchError::Failed(format!(
                            "DeepSeek search failed: {reason}; {exa_error}"
                        ))),
                    }
                }
            }
        }
    }
}

fn format_results(results: Vec<SearchResult>) -> String {
    if results.is_empty() {
        return "(no web search results)".to_string();
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            let published = result
                .published
                .map(|day| format!(" ({day})"))
                .unwrap_or_default();
            format!(
                "{}. {}{published}\n{}\n{}",
                index + 1,
                result.title,
                result.description,
                result.url
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// --- DeepSeek Search API ---

/// The query DeepSeek receives: the recency as `after:`/`before:` operators,
/// with the query text cut so the whole stays within DeepSeek's limit.
fn deepseek_query(query: &str, recency: Option<&Recency>, today: NaiveDate) -> String {
    let query = query.trim();
    let operators = match recency {
        Some(Recency::LastDays(days)) => format!("after:{}", days_before(today, *days)),
        Some(Recency::Range { start, end }) => format!("after:{start} before:{end}"),
        None => return query.chars().take(DEEPSEEK_QUERY_CHARS).collect(),
    };
    let room = DEEPSEEK_QUERY_CHARS.saturating_sub(operators.chars().count() + 1);
    let head: String = query.chars().take(room).collect();
    format!("{} {operators}", head.trim_end())
}

fn deepseek_result(hit: DeepSeekSearchHit) -> SearchResult {
    SearchResult {
        title: hit.title,
        url: hit.url,
        description: truncate_chars(&hit.summary, DEEPSEEK_SUMMARY_CHARS),
        published: hit.date,
    }
}

/// The first `limit` characters of `text`, with `…` when anything was cut.
fn truncate_chars(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

// --- Brave Search API ---

#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: Option<String>,
}

async fn search_brave_or_cancel(
    args: &SearchArgs,
    api_key: &str,
    today: NaiveDate,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<SearchResult>, SearchError> {
    let count = args.count.clamp(1, 10);
    let query_params = brave_query_params(args, count, today);
    let client = search_client()?;
    let response = await_or_cancel(
        client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", api_key)
            .query(&query_params)
            .send(),
        should_cancel,
    )
    .await?
    .map_err(|e| SearchError::Failed(format!("web search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        return Err(SearchError::Failed(format!(
            "web search request failed with {status}"
        )));
    }

    let body: BraveResponse = await_or_cancel(response.json(), should_cancel)
        .await?
        .map_err(|e| SearchError::Failed(format!("invalid web search response: {e}")))?;

    Ok(body
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .take(count)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            description: r.description.unwrap_or_default(),
            published: None,
        })
        .collect())
}

fn brave_query_params(
    args: &SearchArgs,
    count: usize,
    today: NaiveDate,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("q", args.query.clone()),
        ("count", count.clamp(1, 10).to_string()),
    ];
    if let Some(recency) = args.recency.as_ref() {
        params.push(("freshness", brave_freshness(recency, today)));
    }
    params
}

/// Brave's `freshness` value: a named window when one matches, otherwise an
/// explicit date range ending today.
fn brave_freshness(recency: &Recency, today: NaiveDate) -> String {
    match recency {
        Recency::LastDays(1) => "pd".to_string(),
        Recency::LastDays(7) => "pw".to_string(),
        Recency::LastDays(30 | 31) => "pm".to_string(),
        Recency::LastDays(365) => "py".to_string(),
        Recency::LastDays(days) => format!("{}to{today}", days_before(today, *days)),
        Recency::Range { start, end } => format!("{start}to{end}"),
    }
}

fn days_before(today: NaiveDate, days: u32) -> NaiveDate {
    today
        .checked_sub_days(chrono::Days::new(u64::from(days)))
        .unwrap_or(today)
}

// --- Exa MCP fallback (no API key required) ---

async fn search_exa_at_or_cancel_async(
    args: &SearchArgs,
    endpoint: &str,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<SearchResult>, SearchError> {
    let count = args.count.clamp(1, 10);
    let query = exa_query(args);
    let request_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": {
                "query": query,
                "type": "auto",
                "numResults": count
            }
        }
    });

    let client = search_client()?;
    let response = await_or_cancel(
        client
            .post(endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send(),
        should_cancel,
    )
    .await?
    .map_err(|e| SearchError::Failed(format!("Exa search failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        return Err(SearchError::Failed(format!(
            "Exa search failed with {status}"
        )));
    }

    let text = await_or_cancel(response.text(), should_cancel)
        .await?
        .map_err(|e| SearchError::Failed(format!("failed to read Exa response: {e}")))?;

    parse_exa_response(&text).map_err(SearchError::Failed)
}

fn search_client() -> Result<reqwest::Client, SearchError> {
    let builder = orca_mcp::http::client_builder().timeout(SEARCH_TIMEOUT);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|error| {
        SearchError::Failed(format!(
            "failed to build web search client: {}",
            orca_mcp::http::build_error(&error)
        ))
    })
}

async fn await_or_cancel<T>(
    future: impl Future<Output = T>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<T, SearchError> {
    tokio::pin!(future);
    loop {
        if should_cancel() {
            return Err(SearchError::Cancelled);
        }
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
fn execute_exa_at_or_cancel(
    request: &ToolRequest,
    max_bytes: usize,
    endpoint: &str,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test search runtime");
    match runtime.block_on(search_exa_at_or_cancel_async(
        &args,
        endpoint,
        &should_cancel,
    )) {
        Ok(results) => {
            let (output, truncated) = truncate_output(format_results(results), max_bytes);
            ToolResult::completed(request, output, truncated)
        }
        Err(SearchError::Cancelled) => {
            ToolResult::cancelled(request, "web search was cancelled", None)
        }
        Err(SearchError::Failed(error)) => ToolResult::failed(request, error, None),
    }
}

fn parse_exa_response(text: &str) -> Result<Vec<SearchResult>, String> {
    // Response may be SSE (data: {...}) or direct JSON
    let json_str = if let Some(data_line) = text.lines().find(|l| l.starts_with("data: ")) {
        &data_line[6..]
    } else {
        text.trim()
    };

    let response: Value =
        serde_json::from_str(json_str).map_err(|e| format!("invalid Exa response: {e}"))?;

    let content_text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    if content_text.is_empty() {
        return Ok(Vec::new());
    }

    Ok(parse_exa_text_results(content_text))
}

fn parse_exa_text_results(text: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut title = String::new();
    let mut url = String::new();
    let mut snippet = String::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(t) = line.strip_prefix("Title: ") {
            // Save previous result
            if !title.is_empty() && !url.is_empty() {
                results.push(SearchResult {
                    title: std::mem::take(&mut title),
                    url: std::mem::take(&mut url),
                    description: std::mem::take(&mut snippet),
                    published: None,
                });
            }
            title = t.to_string();
            snippet.clear();
        } else if let Some(u) = line.strip_prefix("URL: ") {
            url = u.to_string();
        } else if let Some(s) = line.strip_prefix("Text: ") {
            snippet = s.chars().take(300).collect();
        }
    }

    // Save last result
    if !title.is_empty() && !url.is_empty() {
        results.push(SearchResult {
            title,
            url,
            description: snippet,
            published: None,
        });
    }

    results
}

fn parse_args(request: &ToolRequest) -> Result<SearchArgs, String> {
    let Some(raw) = request.raw_arguments.as_deref() else {
        return request
            .target
            .as_deref()
            .filter(|query| !query.trim().is_empty())
            .map(|query| {
                let recency = infer_recency(query);
                SearchArgs {
                    query: query.to_string(),
                    count: default_count(),
                    recency_inferred: recency.is_some(),
                    recency,
                }
            })
            .ok_or_else(|| "web_search query is required".to_string());
    };
    let args: RawSearchArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.query.trim().is_empty() {
        return Err("web_search query is required".to_string());
    }
    let (recency, recency_inferred) =
        match args.fresh_days.map(Recency::LastDays).or(args.freshness) {
            Some(sent) => (Some(sent), false),
            None => {
                let inferred = infer_recency(&args.query);
                let recency_inferred = inferred.is_some();
                (inferred, recency_inferred)
            }
        };
    Ok(SearchArgs {
        query: args.query,
        count: args.count,
        recency,
        recency_inferred,
    })
}

fn default_count() -> usize {
    5
}

/// A positive whole number of days, sent as a number or a digit string.
/// Anything else is dropped rather than failing the call, and values above
/// [`MAX_FRESH_DAYS`] are clamped.
fn deserialize_fresh_days<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let days = match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    Ok(days
        .filter(|days| *days > 0)
        .and_then(|days| u32::try_from(days.min(i64::from(MAX_FRESH_DAYS))).ok()))
}

fn deserialize_legacy_freshness<'de, D>(deserializer: D) -> Result<Option<Recency>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::String(text)) => legacy_freshness(text.trim()),
        _ => None,
    })
}

fn legacy_freshness(value: &str) -> Option<Recency> {
    match value {
        "pd" => Some(Recency::LastDays(1)),
        "pw" => Some(Recency::LastDays(7)),
        "pm" => Some(Recency::LastDays(31)),
        "py" => Some(Recency::LastDays(MAX_FRESH_DAYS)),
        custom => {
            let (start, end) = custom.split_once("to")?;
            (is_iso_date(start) && is_iso_date(end)).then(|| Recency::Range {
                start: start.to_string(),
                end: end.to_string(),
            })
        }
    }
}

/// The recency a query implies by its wording, used when the model passes
/// none. When words for several windows appear, the shortest window wins.
fn infer_recency(query: &str) -> Option<Recency> {
    let lowered = query.to_ascii_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .collect();
    let has_word = |words: &[&str]| tokens.iter().any(|token| words.contains(token));
    let has_phrase = |phrases: &[&str]| phrases.iter().any(|phrase| query.contains(phrase));

    let days = if has_word(&["today"]) || has_phrase(&["今天"]) {
        1
    } else if has_phrase(&["本周", "这周"]) {
        7
    } else if has_word(&["latest", "current", "recent", "news", "updates", "update"])
        || has_phrase(&["最新", "近期", "新闻"])
    {
        31
    } else {
        return None;
    };
    Some(Recency::LastDays(days))
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7) || byte.is_ascii_digit())
}

fn exa_query(args: &SearchArgs) -> String {
    match &args.recency {
        Some(Recency::LastDays(1)) => format!("{} from the last 24 hours", args.query),
        Some(Recency::LastDays(days)) => format!("{} from the last {days} days", args.query),
        Some(Recency::Range { start, end }) => {
            format!("{} published between {start} and {end}", args.query)
        }
        None => args.query.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn request(raw_arguments: Option<String>, target: Option<String>) -> ToolRequest {
        ToolRequest {
            id: "search-1".to_string(),
            name: ToolName::WebSearch,
            action: ActionKind::Read,
            target,
            raw_arguments,
        }
    }

    #[test]
    fn web_search_cancellation_preempts_http_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind withheld-response server");
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept request");
            let mut request = [0u8; 4096];
            let _ = connection.read(&mut request);
            accepted_tx.send(()).expect("announce accepted request");
            std::thread::sleep(Duration::from_secs(2));
        });
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            execute_exa_at_or_cancel(
                &request(Some(r#"{"query":"rust","count":3}"#.to_string()), None),
                16_384,
                &endpoint,
                || worker_cancelled.load(Ordering::Acquire),
            )
        });

        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("search request reaches withheld server");
        let cancelled_at = Instant::now();
        cancelled.store(true, Ordering::Release);
        let result = worker.join().expect("search worker joins");

        assert_eq!(result.status, ToolStatus::Cancelled);
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(250),
            "cancellation took {:?}",
            cancelled_at.elapsed()
        );
        server.join().expect("withheld-response server joins");
    }

    #[test]
    fn parses_json_args() {
        let args = parse_args(&request(
            Some(r#"{"query":"rust","count":3}"#.to_string()),
            None,
        ))
        .unwrap();
        assert_eq!(args.query, "rust");
        assert_eq!(args.count, 3);
    }

    #[test]
    fn parses_target_fallback() {
        let args = parse_args(&request(None, Some("rust async".to_string()))).unwrap();
        assert_eq!(args.query, "rust async");
        assert_eq!(args.count, 5);
    }

    #[test]
    fn parses_recency_intent_query_with_month_freshness() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        assert_eq!(args.recency, Some(Recency::LastDays(31)));
    }

    #[test]
    fn brave_query_params_include_inferred_freshness() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        let params = brave_query_params(&args, args.count, today());

        assert_eq!(
            params,
            vec![
                ("q", "deepseek latest news".to_string()),
                ("count", "3".to_string()),
                ("freshness", "pm".to_string())
            ]
        );
    }

    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 14).expect("valid test date")
    }

    fn recency_of(raw: &str) -> Option<Recency> {
        parse_args(&request(Some(raw.to_string()), None))
            .expect("arguments parse")
            .recency
    }

    fn range(start: &str, end: &str) -> Recency {
        Recency::Range {
            start: start.to_string(),
            end: end.to_string(),
        }
    }

    #[test]
    fn fresh_days_maps_to_brave_codes_and_custom_ranges() {
        for (days, expected) in [
            (1, "pd"),
            (7, "pw"),
            (30, "pm"),
            (31, "pm"),
            (365, "py"),
            (3, "2026-11-11to2026-11-14"),
        ] {
            assert_eq!(
                brave_freshness(&Recency::LastDays(days), today()),
                expected,
                "{days} days"
            );
        }
        assert_eq!(
            brave_freshness(&range("2026-01-01", "2026-02-01"), today()),
            "2026-01-01to2026-02-01"
        );
    }

    #[test]
    fn fresh_days_accepts_numeric_strings_and_clamps() {
        assert_eq!(
            recency_of(r#"{"query":"x","fresh_days":"7"}"#),
            Some(Recency::LastDays(7))
        );
        assert_eq!(recency_of(r#"{"query":"x","fresh_days":0}"#), None);
        assert_eq!(recency_of(r#"{"query":"x","fresh_days":-3}"#), None);
        assert_eq!(
            recency_of(r#"{"query":"x","fresh_days":9999}"#),
            Some(Recency::LastDays(365))
        );
        assert_eq!(recency_of(r#"{"query":"x","fresh_days":"abc"}"#), None);
    }

    #[test]
    fn legacy_freshness_codes_still_parse() {
        assert_eq!(
            recency_of(r#"{"query":"x","freshness":"pw"}"#),
            Some(Recency::LastDays(7))
        );
        assert_eq!(
            recency_of(r#"{"query":"x","freshness":"pm"}"#),
            Some(Recency::LastDays(31))
        );
        assert_eq!(
            recency_of(r#"{"query":"x","freshness":"2026-01-01to2026-02-01"}"#),
            Some(range("2026-01-01", "2026-02-01"))
        );
    }

    #[test]
    fn fresh_days_wins_over_legacy_freshness() {
        assert_eq!(
            recency_of(r#"{"query":"x","fresh_days":1,"freshness":"py"}"#),
            Some(Recency::LastDays(1))
        );
    }

    #[test]
    fn explicit_fresh_days_wins_over_inferred_keywords() {
        assert_eq!(
            recency_of(r#"{"query":"rust news today","fresh_days":30}"#),
            Some(Recency::LastDays(30))
        );
    }

    #[test]
    fn inferred_recency_understands_today_and_chinese_keywords() {
        assert_eq!(infer_recency("rust news today"), Some(Recency::LastDays(1)));
        assert_eq!(infer_recency("今天 天气"), Some(Recency::LastDays(1)));
        assert_eq!(infer_recency("本周 rust 发布"), Some(Recency::LastDays(7)));
        assert_eq!(
            infer_recency("deepseek 最新 模型"),
            Some(Recency::LastDays(31))
        );
        assert_eq!(
            infer_recency("latest rust release"),
            Some(Recency::LastDays(31))
        );
        assert_eq!(infer_recency("rust ownership"), None);
    }

    #[test]
    fn exa_query_describes_day_windows_and_ranges() {
        let args = |recency| SearchArgs {
            query: "q".to_string(),
            count: 5,
            recency,
            recency_inferred: false,
        };
        assert_eq!(
            exa_query(&args(Some(Recency::LastDays(1)))),
            "q from the last 24 hours"
        );
        assert_eq!(
            exa_query(&args(Some(Recency::LastDays(3)))),
            "q from the last 3 days"
        );
        assert_eq!(
            exa_query(&args(Some(range("2026-01-01", "2026-02-01")))),
            "q published between 2026-01-01 and 2026-02-01"
        );
    }

    /// An Exa endpoint nothing listens on; a test that reaches it fails.
    const UNREACHABLE_EXA: &str = "http://127.0.0.1:9/mcp";

    const EXA_ONE_RESULT: &str = r#"data: {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"Title: Example\nURL: https://example.com/\nText: An example site."}]}}"#;

    const DEEPSEEK_BOOM: &str = r#"{"error":{"message":"boom"}}"#;

    fn search_args(query: &str, count: usize, recency: Option<Recency>) -> SearchArgs {
        SearchArgs {
            query: query.to_string(),
            count,
            recency,
            recency_inferred: false,
        }
    }

    #[test]
    fn deepseek_ignores_inferred_recency() {
        let fixture = crate::test_http::serve(vec![(200, r#"{"results":[]}"#.into())]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");
        let args = parse_args(&request(
            Some(r#"{"query":"rust std::env::current_dir","count":3}"#.to_string()),
            None,
        ))
        .unwrap();
        assert_eq!(
            args.recency,
            Some(Recency::LastDays(31)),
            "Brave and Exa still get the inferred window"
        );

        search_with(SearchBackend::DeepSeek(&access), &args, UNREACHABLE_EXA)
            .expect("search succeeds");

        let requests = fixture.requests();
        let body: Value =
            serde_json::from_str(crate::test_http::body(&requests[0])).expect("JSON request body");
        assert_eq!(body["query"], "rust std::env::current_dir");
    }

    #[test]
    fn ambiguous_chinese_words_do_not_imply_recency() {
        assert_eq!(infer_recency("最近邻 搜索 rust"), None);
        assert_eq!(infer_recency("最近公共祖先 算法"), None);
        assert_eq!(infer_recency("今日头条 开放平台 API"), None);
    }

    fn search_with(
        backend: SearchBackend<'_>,
        args: &SearchArgs,
        exa_endpoint: &str,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test search runtime");
        match runtime.block_on(search_output(args, backend, exa_endpoint, today(), &|| {
            false
        })) {
            Ok(output) => Ok(output),
            Err(SearchError::Cancelled) => Err("cancelled".to_string()),
            Err(SearchError::Failed(error)) => Err(error),
        }
    }

    #[test]
    fn deepseek_search_posts_query_with_recency_operator_and_bearer_key() {
        let fixture = crate::test_http::serve(vec![(200, r#"{"results":[]}"#.into())]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        search_with(
            SearchBackend::DeepSeek(&access),
            &search_args("rust release", 3, Some(Recency::LastDays(7))),
            UNREACHABLE_EXA,
        )
        .expect("search succeeds");

        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request.starts_with("POST /v1/tools/search HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: bearer sk-test\r\n"),
            "{request}"
        );
        let body: Value =
            serde_json::from_str(crate::test_http::body(request)).expect("JSON request body");
        assert_eq!(
            body,
            serde_json::json!({"query": "rust release after:2026-11-07", "count": 3})
        );
    }

    #[test]
    fn deepseek_search_formats_date_and_caps_summary_at_800_chars() {
        let response = serde_json::json!({"results": [{
            "title": "Title",
            "url": "https://example.com/a",
            "summary": "长".repeat(1000),
            "snippet": "short",
            "date": "2026-11-12T16:00:00+00:00"
        }]})
        .to_string();
        let fixture = crate::test_http::serve(vec![(200, response)]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let output = search_with(
            SearchBackend::DeepSeek(&access),
            &search_args("rust", 5, None),
            UNREACHABLE_EXA,
        )
        .expect("search succeeds");
        fixture.requests();

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "1. Title (2026-11-12)");
        assert_eq!(lines[1], format!("{}…", "长".repeat(800)));
        assert_eq!(lines[2], "https://example.com/a");
    }

    #[test]
    fn deepseek_query_with_operators_stays_within_128_chars() {
        let query = "查".repeat(150);

        let built = deepseek_query(&query, Some(&Recency::LastDays(7)), today());

        assert!(
            built.chars().count() <= 128,
            "{} chars",
            built.chars().count()
        );
        assert!(built.ends_with(" after:2026-11-07"), "{built}");
    }

    #[test]
    fn auto_backend_prefers_brave_then_deepseek_then_exa() {
        let access = DeepSeekWebAccess::for_test("http://127.0.0.1:9", "sk-test");

        assert!(matches!(
            choose_search_backend(Some("k"), Some(&access)),
            SearchBackend::Brave("k")
        ));
        assert!(matches!(
            choose_search_backend(Some("k"), None),
            SearchBackend::Brave("k")
        ));
        assert!(matches!(
            choose_search_backend(None, Some(&access)),
            SearchBackend::DeepSeek(_)
        ));
        assert!(matches!(
            choose_search_backend(Some("  "), Some(&access)),
            SearchBackend::DeepSeek(_)
        ));
        assert!(matches!(
            choose_search_backend(None, None),
            SearchBackend::Exa
        ));
        assert!(matches!(
            choose_search_backend(Some("  "), None),
            SearchBackend::Exa
        ));
    }

    #[test]
    fn deepseek_failure_falls_back_to_exa_with_note() {
        let deepseek = crate::test_http::serve(vec![(500, DEEPSEEK_BOOM.into())]);
        let exa = crate::test_http::serve(vec![(200, EXA_ONE_RESULT.into())]);
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let output = search_with(
            SearchBackend::DeepSeek(&access),
            &search_args("rust", 3, None),
            &format!("{}/mcp", exa.origin),
        )
        .expect("the Exa fallback succeeds");

        assert!(
            output.starts_with(
                "[DeepSeek search failed: DeepSeek returned HTTP 500: boom. These results come from Exa.]"
            ),
            "{output}"
        );
        assert!(
            output.contains("1. Example\nAn example site.\nhttps://example.com/"),
            "{output}"
        );
        assert_eq!(deepseek.requests().len(), 1);
        assert_eq!(exa.requests().len(), 1);
    }

    #[test]
    fn deepseek_and_exa_failures_are_reported_together() {
        let deepseek = crate::test_http::serve(vec![(500, DEEPSEEK_BOOM.into())]);
        let exa = crate::test_http::serve(vec![(503, "{}".into())]);
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let error = search_with(
            SearchBackend::DeepSeek(&access),
            &search_args("rust", 3, None),
            &format!("{}/mcp", exa.origin),
        )
        .expect_err("both backends fail");

        assert!(
            error.starts_with("DeepSeek search failed: DeepSeek returned HTTP 500: boom; "),
            "{error}"
        );
        assert!(error.contains("Exa search failed with 503"), "{error}");
        deepseek.requests();
        exa.requests();
    }

    #[test]
    fn empty_deepseek_results_do_not_fall_back() {
        let deepseek = crate::test_http::serve(vec![(200, r#"{"results":[]}"#.into())]);
        let exa = TcpListener::bind("127.0.0.1:0").expect("bind Exa listener");
        let exa_endpoint = format!("http://{}/mcp", exa.local_addr().unwrap());
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let output = search_with(
            SearchBackend::DeepSeek(&access),
            &search_args("rust", 3, None),
            &exa_endpoint,
        )
        .expect("search succeeds");

        assert_eq!(output, "(no web search results)");
        deepseek.requests();
        exa.set_nonblocking(true)
            .expect("non-blocking Exa listener");
        assert!(
            matches!(exa.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "Exa must not be contacted"
        );
    }

    #[test]
    fn exa_query_includes_recency_window_when_backend_has_no_freshness_arg() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        assert_eq!(
            exa_query(&args),
            "deepseek latest news from the last 31 days"
        );
    }

    #[test]
    fn parse_exa_text_results_extracts_entries() {
        let text = "\
Title: Rust Programming Language
URL: https://www.rust-lang.org/
Text: A language empowering everyone to build reliable software.

Title: Rust Documentation
URL: https://doc.rust-lang.org/
Text: Official Rust documentation and guides.";
        let results = parse_exa_text_results(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            results[0].description,
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(results[1].url, "https://doc.rust-lang.org/");
        assert_eq!(results[1].title, "Rust Documentation");
    }

    #[test]
    fn parse_exa_response_handles_sse_format() {
        let sse = r#"data: {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"Title: Example\nURL: https://example.com/\nText: An example site."}]}}"#;
        let results = parse_exa_response(sse).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example");
        assert_eq!(results[0].url, "https://example.com/");
    }
}
