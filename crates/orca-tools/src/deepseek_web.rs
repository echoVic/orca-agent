//! DeepSeek's Search and Fetch APIs, which `web_search` and `web_fetch` use
//! when Orca talks to the official DeepSeek endpoint.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use orca_core::config::ProviderKind;
use serde::Deserialize;
use serde_json::{Value, json};

/// The only origin the web tools ever send the DeepSeek API key to.
const DEEPSEEK_ORIGIN: &str = "https://api.deepseek.com";
const DEEPSEEK_HOST: &str = "api.deepseek.com";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const CANCEL_POLL: Duration = Duration::from_millis(10);
/// How much of DeepSeek's own error message an error carries.
const ERROR_MESSAGE_CHARS: usize = 200;

/// Permission for the web tools to call DeepSeek's web APIs with the
/// session's API key.
#[derive(Clone)]
pub struct DeepSeekWebAccess {
    origin: String,
    api_key: String,
}

impl DeepSeekWebAccess {
    /// The access a session gets, or `None` when its tools must not call
    /// DeepSeek: the switch is off, the provider is not DeepSeek, there is no
    /// key, or the endpoint is anything other than the official one over
    /// https on the default port. A custom endpoint usually carries another
    /// provider's key, and that key must never reach DeepSeek.
    pub fn for_provider(
        provider: ProviderKind,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Option<Self> {
        if !enabled || provider != ProviderKind::DeepSeek {
            return None;
        }
        let api_key = api_key.map(str::trim).filter(|key| !key.is_empty())?;
        if base_url.is_some_and(|base_url| !is_official_endpoint(base_url)) {
            return None;
        }
        Some(Self {
            origin: DEEPSEEK_ORIGIN.to_string(),
            api_key: api_key.to_string(),
        })
    }

    /// Access that points at a local test server instead of DeepSeek.
    #[cfg(test)]
    pub(crate) fn for_test(origin: &str, api_key: &str) -> Self {
        Self {
            origin: origin.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }
}

fn is_official_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url.trim()).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(DEEPSEEK_HOST))
            && url.port().is_none()
    })
}

impl fmt::Debug for DeepSeekWebAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepSeekWebAccess")
            .field("origin", &self.origin)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Why a DeepSeek web call returned nothing usable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeepSeekWebError {
    Cancelled,
    /// A message fit to show the model; it never contains the API key.
    Failed(String),
}

/// One result of the Search API.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeepSeekSearchHit {
    pub(crate) title: String,
    pub(crate) url: String,
    /// The long summary written for models, or the snippet when it is empty.
    pub(crate) summary: String,
    /// The publication day as `YYYY-MM-DD`, when DeepSeek knows it.
    pub(crate) date: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<RawSearchHit>,
}

#[derive(Deserialize)]
struct RawSearchHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

/// Runs one Search API request. `query` must already fit DeepSeek's
/// 128-character limit, operators included.
pub(crate) async fn search(
    access: &DeepSeekWebAccess,
    query: &str,
    count: usize,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<DeepSeekSearchHit>, DeepSeekWebError> {
    let body = json!({ "query": query, "count": count.clamp(1, 10) });
    let text = post_json(
        access,
        "/v1/tools/search",
        &body,
        SEARCH_TIMEOUT,
        should_cancel,
    )
    .await?;
    let response: SearchResponse = serde_json::from_str(&text).map_err(|error| {
        DeepSeekWebError::Failed(format!("invalid DeepSeek search response: {error}"))
    })?;
    Ok(response
        .results
        .into_iter()
        .filter_map(|hit| {
            let url = hit.url.filter(|url| !url.trim().is_empty())?;
            let summary = hit
                .summary
                .filter(|summary| !summary.trim().is_empty())
                .or(hit.snippet)
                .unwrap_or_default();
            Some(DeepSeekSearchHit {
                title: hit.title.unwrap_or_default(),
                url,
                summary,
                date: hit.date.as_deref().and_then(publication_day),
            })
        })
        .collect())
}

/// `YYYY-MM-DD` from the front of an RFC 3339 timestamp.
fn publication_day(date: &str) -> Option<String> {
    let day = date.get(..10)?;
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .map(|_| day.to_string())
}

/// A page the Fetch API read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeepSeekPage {
    /// Empty when DeepSeek found no title.
    pub(crate) title: String,
    /// The page as Markdown; never blank.
    pub(crate) content: String,
}

#[derive(Deserialize)]
struct FetchResponse {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// Reads one page through the Fetch API. A page with no content counts as a
/// failure.
pub(crate) async fn fetch(
    access: &DeepSeekWebAccess,
    url: &str,
    should_cancel: &dyn Fn() -> bool,
) -> Result<DeepSeekPage, DeepSeekWebError> {
    let body = json!({ "url": url });
    let text = post_json(
        access,
        "/v1/tools/fetch",
        &body,
        FETCH_TIMEOUT,
        should_cancel,
    )
    .await?;
    let response: FetchResponse = serde_json::from_str(&text).map_err(|error| {
        DeepSeekWebError::Failed(format!("invalid DeepSeek Fetch response: {error}"))
    })?;
    let content = response.content.unwrap_or_default();
    if content.trim().is_empty() {
        return Err(DeepSeekWebError::Failed(
            "DeepSeek Fetch returned no content".to_string(),
        ));
    }
    Ok(DeepSeekPage {
        title: response.title.unwrap_or_default(),
        content,
    })
}

/// POSTs `body` to `path` on the access's origin and returns the response
/// body of a successful request.
async fn post_json(
    access: &DeepSeekWebAccess,
    path: &str,
    body: &Value,
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
) -> Result<String, DeepSeekWebError> {
    let builder = orca_mcp::http::client_builder().timeout(timeout);
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder.build().map_err(|error| {
        DeepSeekWebError::Failed(format!(
            "failed to build the DeepSeek client: {}",
            orca_mcp::http::build_error(&error)
        ))
    })?;
    let response = await_or_cancel(
        client
            .post(format!("{}{path}", access.origin))
            .bearer_auth(&access.api_key)
            .json(body)
            .send(),
        should_cancel,
    )
    .await?
    .map_err(|error| DeepSeekWebError::Failed(format!("DeepSeek request failed: {error}")))?;
    let status = response.status().as_u16();
    let text = await_or_cancel(response.text(), should_cancel)
        .await?
        .map_err(|error| {
            DeepSeekWebError::Failed(format!("failed to read the DeepSeek response: {error}"))
        })?;
    if !(200..300).contains(&status) {
        return Err(DeepSeekWebError::Failed(http_error_message(status, &text)));
    }
    Ok(text)
}

fn http_error_message(status: u16, body: &str) -> String {
    match status {
        401 => "DeepSeek rejected the API key (HTTP 401)".to_string(),
        402 => "the DeepSeek account balance is too low (HTTP 402)".to_string(),
        429 => "DeepSeek rate limit reached (HTTP 429)".to_string(),
        _ => {
            let message: String = serde_json::from_str::<Value>(body)
                .ok()
                .as_ref()
                .and_then(|value| value.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .chars()
                .take(ERROR_MESSAGE_CHARS)
                .collect();
            if message.is_empty() {
                format!("DeepSeek returned HTTP {status}")
            } else {
                format!("DeepSeek returned HTTP {status}: {message}")
            }
        }
    }
}

async fn await_or_cancel<T>(
    future: impl Future<Output = T>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<T, DeepSeekWebError> {
    tokio::pin!(future);
    loop {
        if should_cancel() {
            return Err(DeepSeekWebError::Cancelled);
        }
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(CANCEL_POLL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::ProviderKind;

    fn for_provider(
        provider: ProviderKind,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Option<DeepSeekWebAccess> {
        DeepSeekWebAccess::for_provider(provider, base_url, api_key, enabled)
    }

    #[test]
    fn access_requires_official_https_endpoint_and_a_key() {
        let key = Some("sk-x");
        assert!(for_provider(ProviderKind::DeepSeek, None, key, true).is_some());
        for allowed in ["https://api.deepseek.com/beta", "https://API.DeepSeek.com"] {
            assert!(
                for_provider(ProviderKind::DeepSeek, Some(allowed), key, true).is_some(),
                "{allowed} should be allowed"
            );
        }
        for refused in [
            "http://api.deepseek.com",
            "https://api.deepseek.com.evil.example",
            "https://api.deepseek.com:8443",
            "https://api.deepseek.com@evil.example/",
            "https://api.siliconflow.cn/v1",
        ] {
            assert!(
                for_provider(ProviderKind::DeepSeek, Some(refused), key, true).is_none(),
                "{refused} should be refused"
            );
        }
        assert!(for_provider(ProviderKind::DeepSeek, None, None, true).is_none());
        assert!(for_provider(ProviderKind::DeepSeek, None, Some("  "), true).is_none());
        assert!(for_provider(ProviderKind::DeepSeek, None, key, false).is_none());
        assert!(for_provider(ProviderKind::Mock, None, key, true).is_none());
    }

    #[test]
    fn access_debug_redacts_the_key() {
        let access = for_provider(ProviderKind::DeepSeek, None, Some("sk-x"), true)
            .expect("official endpoint access");
        let debug = format!("{access:?}");

        assert!(!debug.contains("sk-x"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    #[test]
    fn deepseek_http_errors_map_to_readable_messages() {
        for (status, expected) in [
            (401, "DeepSeek rejected the API key (HTTP 401)"),
            (402, "the DeepSeek account balance is too low (HTTP 402)"),
            (429, "DeepSeek rate limit reached (HTTP 429)"),
            (500, "DeepSeek returned HTTP 500: boom"),
        ] {
            let fixture =
                crate::test_http::serve(vec![(status, r#"{"error":{"message":"boom"}}"#.into())]);
            let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

            let outcome = block_on(search(&access, "rust", 3, &|| false));

            assert_eq!(
                outcome,
                Err(DeepSeekWebError::Failed(expected.to_string())),
                "HTTP {status}"
            );
            assert_eq!(fixture.requests().len(), 1);
        }
    }

    #[test]
    fn deepseek_fetch_posts_url_and_returns_markdown() {
        let fixture = crate::test_http::serve(vec![(
            200,
            r##"{"title":"A","content":"# A\n\nbody"}"##.into(),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let page = block_on(fetch(&access, "https://example.com/a", &|| false));

        assert_eq!(
            page,
            Ok(DeepSeekPage {
                title: "A".to_string(),
                content: "# A\n\nbody".to_string(),
            })
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request.starts_with("POST /v1/tools/fetch HTTP/1.1\r\n"),
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
        assert_eq!(body, json!({"url": "https://example.com/a"}));
    }

    #[test]
    fn deepseek_fetch_without_content_is_a_failure() {
        let fixture =
            crate::test_http::serve(vec![(200, r#"{"title":"A","content":"  "}"#.into())]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let page = block_on(fetch(&access, "https://example.com/a", &|| false));

        assert_eq!(
            page,
            Err(DeepSeekWebError::Failed(
                "DeepSeek Fetch returned no content".to_string()
            ))
        );
        fixture.requests();
    }
}
