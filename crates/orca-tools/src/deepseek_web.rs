//! DeepSeek's Search, Fetch and OCR APIs, which `web_search`, `web_fetch`
//! and `read_file` use when Orca talks to the official DeepSeek endpoint.

use std::fmt;
use std::future::Future;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_core::config::ProviderKind;
use serde::Deserialize;
use serde_json::{Value, json};

/// The only origin the web tools ever send the DeepSeek API key to.
const DEEPSEEK_ORIGIN: &str = "https://api.deepseek.com";
const DEEPSEEK_HOST: &str = "api.deepseek.com";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// DeepSeek gives a recognition 180 s from the start of the upload.
const OCR_TIMEOUT: Duration = Duration::from_secs(180);
/// Word and PowerPoint text is extracted directly; everything else is
/// recognized page by page.
const OCR_PURPOSE: &str = "text_recognition";
const CANCEL_POLL: Duration = Duration::from_millis(10);
/// How much of DeepSeek's own error message an error carries.
const ERROR_MESSAGE_CHARS: usize = 200;

/// Permission for the web tools and `read_file` to call DeepSeek's web and
/// OCR APIs with the session's API key.
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

/// One page DeepSeek OCR recognized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OcrPage {
    /// The page text without layout tokens.
    pub(crate) text: String,
    /// False when the page reached DeepSeek's output limit, so its text is
    /// cut short.
    pub(crate) complete: bool,
}

#[derive(Deserialize)]
struct OcrResponse {
    #[serde(default)]
    pages: Vec<RawOcrPage>,
}

#[derive(Deserialize)]
struct RawOcrPage {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Recognizes the text of one document or image, page by page, in page
/// order. DeepSeek bills each page, and does so even when the request is
/// cancelled once the upload is done.
pub(crate) async fn ocr(
    access: &DeepSeekWebAccess,
    file_name: &str,
    bytes: Vec<u8>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<OcrPage>, DeepSeekWebError> {
    let boundary = form_boundary(&bytes);
    let form = ocr_form(&boundary, file_name, &bytes);
    drop(bytes);
    let text = post(
        access,
        "/v1/tools/ocr",
        OCR_TIMEOUT,
        should_cancel,
        |request| {
            request
                .header(
                    reqwest::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(form)
        },
    )
    .await?;
    // The keep-alive newlines before the result are JSON whitespace.
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        DeepSeekWebError::Failed(format!("invalid DeepSeek OCR response: {error}"))
    })?;
    // A recognition that fails after it started still answers HTTP 200,
    // with DeepSeek's reason, such as an unsupported format, in the body.
    if value.get("error").is_some_and(|error| !error.is_null()) {
        let message = deepseek_message(&text, &access.api_key);
        return Err(DeepSeekWebError::Failed(if message.is_empty() {
            "the recognition failed without a reason".to_string()
        } else {
            message
        }));
    }
    let response: OcrResponse = serde_json::from_value(value).map_err(|error| {
        DeepSeekWebError::Failed(format!("invalid DeepSeek OCR response: {error}"))
    })?;
    let mut pages = response.pages;
    pages.sort_by_key(|page| page.index);
    Ok(pages
        .into_iter()
        .map(|page| OcrPage {
            text: ocr_text(page.text.as_deref().unwrap_or_default()),
            complete: page.finish_reason.as_deref() != Some("length"),
        })
        .collect())
}

/// Layout spans: a region's coordinates, which mean nothing to a model.
static LAYOUT_SPAN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?s)<[|｜](?:box|det)[|｜]>.*?<[|｜]/(?:box|det)[|｜]>")
        .expect("layout span pattern")
});
/// The tags around a region's text, whose text stays.
static REFERENCE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[|｜]/?ref[|｜]>").expect("reference tag pattern"));

/// A page's text without DeepSeek OCR's layout tokens: each
/// `<｜box｜>[[x1, y1, x2, y2]]<｜/box｜>` span goes, and the `<｜ref｜>` tags
/// around the text it locates are unwrapped.
pub(crate) fn ocr_text(raw: &str) -> String {
    REFERENCE_TAG
        .replace_all(&LAYOUT_SPAN.replace_all(raw, ""), "")
        .into_owned()
}

/// A form boundary that does not occur in `bytes`.
fn form_boundary(bytes: &[u8]) -> String {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    (0_u32..)
        .map(|attempt| format!("orca-ocr-{seed:x}-{attempt}"))
        .find(|boundary| {
            regex::bytes::Regex::new(&regex::escape(boundary))
                .is_ok_and(|pattern| !pattern.is_match(bytes))
        })
        .expect("a boundary that is not in the file")
}

/// The OCR request body: the `purpose` field and the `file` field, which
/// are the only two DeepSeek accepts.
fn ocr_form(boundary: &str, file_name: &str, bytes: &[u8]) -> Vec<u8> {
    // A quote or a line break would end the header early, so they are
    // percent-encoded the way browsers send them.
    let file_name = file_name
        .replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A");
    let mut form = Vec::with_capacity(bytes.len() + 512);
    form.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{OCR_PURPOSE}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    form.extend_from_slice(bytes);
    form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    form
}

/// POSTs `body` as JSON to `path` on the access's origin and returns the
/// response body of a successful request.
async fn post_json(
    access: &DeepSeekWebAccess,
    path: &str,
    body: &Value,
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
) -> Result<String, DeepSeekWebError> {
    post(access, path, timeout, should_cancel, |request| {
        request.json(body)
    })
    .await
}

/// POSTs to `path` on the access's origin, with the body `with_body` adds,
/// and returns the response body of a successful request.
async fn post(
    access: &DeepSeekWebAccess,
    path: &str,
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
    with_body: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<String, DeepSeekWebError> {
    // A redirect would carry the request, and with it the key, somewhere
    // other than the fixed origin; it is reported as an error instead.
    let builder = orca_mcp::http::client_builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder.build().map_err(|error| {
        DeepSeekWebError::Failed(format!(
            "failed to build the DeepSeek client: {}",
            orca_mcp::http::build_error(&error)
        ))
    })?;
    let response = await_or_cancel(
        with_body(
            client
                .post(format!("{}{path}", access.origin))
                .bearer_auth(&access.api_key),
        )
        .send(),
        should_cancel,
    )
    .await?
    .map_err(|error| DeepSeekWebError::Failed(describe_request_error(&error)))?;
    let status = response.status().as_u16();
    let text = await_or_cancel(response.text(), should_cancel)
        .await?
        .map_err(|error| {
            if error.is_timeout() {
                DeepSeekWebError::Failed("DeepSeek request timed out".to_string())
            } else {
                DeepSeekWebError::Failed(format!(
                    "failed to read the DeepSeek response: {}",
                    with_causes(&error)
                ))
            }
        })?;
    if !(200..300).contains(&status) {
        return Err(DeepSeekWebError::Failed(http_error_message(
            status,
            &text,
            &access.api_key,
        )));
    }
    Ok(text)
}

/// A readable message for a failed status. DeepSeek's own message is shown
/// with the key masked, since an error can echo what the request sent.
fn http_error_message(status: u16, body: &str, api_key: &str) -> String {
    match status {
        401 => "DeepSeek rejected the API key (HTTP 401)".to_string(),
        402 => "the DeepSeek account balance is too low (HTTP 402)".to_string(),
        429 => "DeepSeek rate limit reached (HTTP 429)".to_string(),
        _ => {
            let message = deepseek_message(body, api_key);
            if message.is_empty() {
                format!("DeepSeek returned HTTP {status}")
            } else {
                format!("DeepSeek returned HTTP {status}: {message}")
            }
        }
    }
}

/// DeepSeek's own `error.message` in `body`, with the key masked and cut to
/// a readable length; empty when there is none.
fn deepseek_message(body: &str, api_key: &str) -> String {
    let raw = serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let masked = if api_key.is_empty() {
        raw
    } else {
        raw.replace(api_key, "<redacted>")
    };
    masked.chars().take(ERROR_MESSAGE_CHARS).collect()
}

/// What went wrong with a request that got no response: a timeout, a failed
/// connection, or something else, with the underlying causes. reqwest's own
/// message names only the URL.
fn describe_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "DeepSeek request timed out".to_string()
    } else if error.is_connect() {
        format!("could not connect to DeepSeek: {}", with_causes(error))
    } else {
        format!("DeepSeek request failed: {}", with_causes(error))
    }
}

/// `error` followed by each of its sources.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
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

    #[test]
    fn deepseek_redirects_are_not_followed() {
        let elsewhere = crate::test_http::untouched();
        let fixture = crate::test_http::serve_replies(vec![crate::test_http::Reply::redirect(
            302,
            &format!("{}/collect", elsewhere.origin),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let outcome = block_on(search(&access, "rust", 3, &|| false));

        assert_eq!(
            outcome,
            Err(DeepSeekWebError::Failed(
                "DeepSeek returned HTTP 302".to_string()
            ))
        );
        elsewhere.assert_untouched("the redirect target");
        fixture.requests();
    }

    #[test]
    fn deepseek_error_messages_never_echo_the_key() {
        let fixture = crate::test_http::serve(vec![(
            500,
            r#"{"error":{"message":"key sk-test is not valid for this account"}}"#.into(),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let outcome = block_on(search(&access, "rust", 3, &|| false));

        assert_eq!(
            outcome,
            Err(DeepSeekWebError::Failed(
                "DeepSeek returned HTTP 500: key <redacted> is not valid for this account"
                    .to_string()
            ))
        );
        fixture.requests();
    }

    #[test]
    fn deepseek_network_errors_say_what_went_wrong() {
        // Nothing listens on the discard port, so connecting fails at once. A
        // port bound and freed by the test could be held by a process another
        // test spawns, which made the connect succeed.
        let access = DeepSeekWebAccess::for_test("http://127.0.0.1:9", "sk-test");

        let refused = block_on(search(&access, "rust", 3, &|| false));

        assert!(
            matches!(&refused, Err(DeepSeekWebError::Failed(message)) if message.starts_with("could not connect to DeepSeek: ")),
            "{refused:?}"
        );

        let silent = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&silent.origin, "sk-test");

        let stalled = block_on(post_json(
            &access,
            "/v1/tools/search",
            &json!({}),
            Duration::from_millis(200),
            &|| false,
        ));

        assert_eq!(
            stalled,
            Err(DeepSeekWebError::Failed(
                "DeepSeek request timed out".to_string()
            ))
        );
    }

    #[test]
    fn the_key_is_masked_before_the_message_is_cut() {
        // The key starts at character 196 of DeepSeek's message, so cutting
        // first would leave "sk-t" unmasked.
        let message = format!("{} sk-test and more", "x".repeat(195));
        let fixture = crate::test_http::serve(vec![(
            500,
            json!({"error": {"message": message}}).to_string(),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let outcome = block_on(search(&access, "rust", 3, &|| false));

        let Err(DeepSeekWebError::Failed(text)) = outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(!text.contains("sk-t"), "{text}");
        assert!(text.ends_with(" <red"), "{text}");
        fixture.requests();
    }

    /// The parts of a `multipart/form-data` request: each part's headers and
    /// its content.
    fn form_parts(request: &str) -> Vec<(String, String)> {
        let boundary = request
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with("content-type: multipart/form-data; boundary=")
            })
            .and_then(|line| line.split_once("boundary="))
            .map(|(_, boundary)| boundary.trim().to_string())
            .expect("a multipart/form-data content type with a boundary");
        let delimiter = format!("--{boundary}");
        let mut pieces = crate::test_http::body(request).split(delimiter.as_str());
        assert_eq!(pieces.next(), Some(""), "the body starts with a delimiter");
        let mut parts = Vec::new();
        let mut closed = false;
        for piece in pieces {
            if piece == "--\r\n" {
                closed = true;
                break;
            }
            let piece = piece
                .strip_prefix("\r\n")
                .and_then(|piece| piece.strip_suffix("\r\n"))
                .unwrap_or_else(|| panic!("a part framed by CRLFs: {piece:?}"));
            let (headers, content) = piece
                .split_once("\r\n\r\n")
                .unwrap_or_else(|| panic!("a part with headers: {piece:?}"));
            parts.push((headers.to_string(), content.to_string()));
        }
        assert!(closed, "the body ends with the closing delimiter");
        parts
    }

    #[test]
    fn ocr_posts_multipart_file_and_purpose() {
        let result = json!({
            "object": "ocr.result",
            "purpose": "text_recognition",
            "page_count": 1,
            "pages": [{"index": 0, "text": "Hello", "finish_reason": "stop"}],
        });
        // DeepSeek sends newlines to keep the connection open until the
        // result is ready.
        let fixture = crate::test_http::serve(vec![(200, format!("\n\n\n{result}"))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let pages = block_on(ocr(
            &access,
            "report.pdf",
            b"%PDF-1.4 fixture".to_vec(),
            &|| false,
        ));

        assert_eq!(
            pages,
            Ok(vec![OcrPage {
                text: "Hello".to_string(),
                complete: true,
            }])
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request.starts_with("POST /v1/tools/ocr HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: bearer sk-test\r\n"),
            "{request}"
        );
        // DeepSeek refuses a form with any other field.
        let parts = form_parts(request);
        assert_eq!(
            parts,
            [
                (
                    "Content-Disposition: form-data; name=\"purpose\"".to_string(),
                    "text_recognition".to_string(),
                ),
                (
                    "Content-Disposition: form-data; name=\"file\"; filename=\"report.pdf\"\r\nContent-Type: application/octet-stream".to_string(),
                    "%PDF-1.4 fixture".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn ocr_file_names_cannot_break_out_of_their_header() {
        let fixture = crate::test_http::serve(vec![(200, r#"{"pages":[]}"#.into())]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let pages = block_on(ocr(&access, "a\"b\r\nc.pdf", b"%PDF".to_vec(), &|| false));

        assert_eq!(pages, Ok(Vec::new()));
        let parts = form_parts(&fixture.requests()[0]);
        assert!(
            parts[1].0.contains("filename=\"a%22b%0D%0Ac.pdf\""),
            "{parts:?}"
        );
    }

    #[test]
    fn ocr_pages_keep_their_order_and_say_when_one_is_cut_short() {
        let fixture = crate::test_http::serve(vec![(
            200,
            json!({"pages": [
                {"index": 1, "text": "second", "finish_reason": "length"},
                {"index": 0, "text": "first", "finish_reason": "stop"},
            ]})
            .to_string(),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let pages = block_on(ocr(&access, "a.pdf", b"%PDF".to_vec(), &|| false));

        assert_eq!(
            pages,
            Ok(vec![
                OcrPage {
                    text: "first".to_string(),
                    complete: true,
                },
                OcrPage {
                    text: "second".to_string(),
                    complete: false,
                },
            ])
        );
        fixture.requests();
    }

    #[test]
    fn ocr_reports_an_error_sent_after_the_status() {
        // Once recognition starts the status is already 200, so a failure
        // arrives as an error object in the body.
        let fixture = crate::test_http::serve(vec![(
            200,
            "\n\n{\"error\":{\"message\":\"OCR did not finish in time\",\"type\":\"gateway_timeout_error\"}}"
                .into(),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let pages = block_on(ocr(&access, "a.pdf", b"%PDF".to_vec(), &|| false));

        assert_eq!(
            pages,
            Err(DeepSeekWebError::Failed(
                "OCR did not finish in time".to_string()
            ))
        );
        fixture.requests();
    }

    #[test]
    fn ocr_text_drops_layout_tokens() {
        assert_eq!(
            ocr_text("<｜ref｜>Title<｜/ref｜><｜box｜>[[52, 40, 948, 96]]<｜/box｜>"),
            "Title"
        );
        assert_eq!(
            ocr_text("<|ref|>Figure 1<|/ref|><|det|>[[1, 2, 3, 4]]<|/det|>\nbody"),
            "Figure 1\nbody"
        );
        assert_eq!(ocr_text("plain\ntext"), "plain\ntext");
    }
}
