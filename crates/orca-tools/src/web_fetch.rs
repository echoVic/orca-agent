//! WebFetch tool — fetch a URL and return its readable text content.
//!
//! Design (informed by Claude Code's WebFetchTool and web_search.rs patterns):
//!
//! - HTTP via `orca_mcp::http::client_builder()` (same client as web_search)
//! - HTML → readable text via lightweight regex-based extraction; no extra crate
//! - Manual redirect handling: same-origin only (≤ MAX_REDIRECTS hops), mirroring
//!   Claude Code's `isPermittedRedirect` guard
//! - 60 s fetch timeout (matches Claude Code's FETCH_TIMEOUT_MS)
//! - 8 MiB body cap, 100 000 character output truncation (matches CC MAX_MARKDOWN_LENGTH)
//! - Optional `prompt` parameter: when given, only paragraphs/lines containing
//!   at least one keyword are returned (simple keyword filter, no secondary model)
//! - Cancellable via the same poll-loop used by web_search

use std::future::Future;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

use crate::deepseek_web::{self, DeepSeekPage, DeepSeekWebAccess, DeepSeekWebError};

// ── limits ─────────────────────────────────────────────────────────────────────

/// HTTP request timeout — matches Claude Code's FETCH_TIMEOUT_MS (60 s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum response body bytes we read (8 MiB).
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum output characters returned to the model — matches Claude Code's
/// MAX_MARKDOWN_LENGTH (100 000).
const MAX_OUTPUT_CHARS: usize = 100_000;

/// Maximum same-origin redirect hops — matches Claude Code's MAX_REDIRECTS (10).
const MAX_REDIRECTS: usize = 10;

/// Cancellation poll interval — same value used by web_search.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// Sent with every direct request. GitHub's API, crates.io and Wikipedia
/// answer requests without a User-Agent with 403.
const USER_AGENT: &str = "Orca (+https://github.com/echoVic/orca-agent)";

// ── public entry points ────────────────────────────────────────────────────────

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
        Ok(a) => a,
        Err(e) => return ToolResult::failed(request, e, None),
    };

    if should_cancel() {
        return ToolResult::cancelled_before_start(request, "web fetch was cancelled");
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return ToolResult::failed(
                request,
                format!("failed to start web fetch runtime: {e}"),
                None,
            );
        }
    };

    let outcome = rt.block_on(fetch_page(&args, deepseek, &should_cancel));

    match outcome {
        Ok(fetched) => {
            let text = render(fetched, args.prompt.as_deref());
            let effective_max = max_bytes.min(MAX_OUTPUT_CHARS);
            let (output, truncated) = truncate_output(text, effective_max);
            ToolResult::completed(request, output, truncated)
        }
        Err(FetchError::Cancelled) => {
            ToolResult::cancelled(request, "web fetch was cancelled", None)
        }
        Err(FetchError::Rejected(message) | FetchError::Failed { message, .. }) => {
            ToolResult::failed(request, message, None)
        }
    }
}

// ── argument schema ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    /// Optional topic or question; when provided, the result is filtered to
    /// paragraphs/lines containing at least one keyword.
    #[serde(default)]
    prompt: Option<String>,
}

fn parse_args(request: &ToolRequest) -> Result<FetchArgs, String> {
    let raw = request.raw_arguments.as_deref().unwrap_or("{}").trim();
    serde_json::from_str::<FetchArgs>(raw).map_err(|e| format!("invalid web_fetch arguments: {e}"))
}

// ── fetch pipeline ─────────────────────────────────────────────────────────────

#[derive(Debug)]
enum FetchError {
    Cancelled,
    /// Orca's own URL or redirect rules refused the request. It is never
    /// retried elsewhere: the rules decide where a fetch may go.
    Rejected(String),
    /// The page could not be read.
    Failed {
        /// The full error, reported when nothing else answers.
        message: String,
        /// A few words for the retry note: `HTTP 403`, `timed out`, …
        reason: String,
        /// The HTTP status, when the server answered with one.
        status: Option<u16>,
    },
}

impl FetchError {
    /// A failure without an HTTP status.
    fn failed(message: impl Into<String>, reason: &str) -> Self {
        Self::Failed {
            message: message.into(),
            reason: reason.to_string(),
            status: None,
        }
    }

    /// A response with an error status.
    fn status(status: u16, message: String) -> Self {
        Self::Failed {
            message,
            reason: format!("HTTP {status}"),
            status: Some(status),
        }
    }
}

/// What `web_fetch` returns: the page text, and a note when the text did not
/// come from the direct request.
#[derive(Debug)]
struct Fetched {
    note: Option<String>,
    text: String,
}

impl Fetched {
    fn direct(text: String) -> Self {
        Self { note: None, text }
    }
}

/// The tool output: the `prompt` filter applies to the page text only, so the
/// note always stays first.
fn render(fetched: Fetched, prompt: Option<&str>) -> String {
    let text = apply_prompt_filter(&fetched.text, prompt);
    match fetched.note {
        Some(note) => format!("{note}\n\n{text}"),
        None => text,
    }
}

async fn fetch_page(
    args: &FetchArgs,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Fetched, FetchError> {
    let url = normalise_url(&args.url)
        .ok_or_else(|| FetchError::Rejected(format!("invalid or unsupported URL: {}", args.url)))?;
    let local = fetch_or_cancel(&url, should_cancel).await;
    finish_with_deepseek(&url, local, deepseek, should_cancel).await
}

async fn fetch_or_cancel(
    url: &str,
    should_cancel: &dyn Fn() -> bool,
) -> Result<LocalPage, FetchError> {
    let client = build_client()?;
    let body = fetch_with_redirects(&client, url, should_cancel, 0).await?;
    Ok(read_page(&body))
}

/// The direct request's outcome, or DeepSeek's copy of the page when the
/// direct request failed in a way DeepSeek may get past. A blank page counts
/// as a failure here, but without a successful retry it is returned as is.
async fn finish_with_deepseek(
    url: &str,
    local: Result<LocalPage, FetchError>,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Fetched, FetchError> {
    let direct = |page: LocalPage| Fetched::direct(page.text);
    let Some(access) = deepseek else {
        return local.map(direct);
    };
    let reason = match &local {
        Ok(page) if !page.readable && is_public_host(url) => "no readable text".to_string(),
        Err(error @ FetchError::Failed { reason, .. })
            if should_retry_with_deepseek(error, url) =>
        {
            reason.clone()
        }
        _ => return local.map(direct),
    };
    match deepseek_web::fetch(access, url, should_cancel).await {
        Ok(page) => Ok(Fetched {
            note: Some(format!(
                "[The direct request failed ({reason}), so this page was fetched through the DeepSeek Fetch API.]"
            )),
            text: page_text(page),
        }),
        Err(DeepSeekWebError::Cancelled) => Err(FetchError::Cancelled),
        Err(DeepSeekWebError::Failed(deepseek_error)) => match local {
            Err(FetchError::Failed {
                message,
                reason,
                status,
            }) => Err(FetchError::Failed {
                message: format!("{message}; DeepSeek Fetch also failed: {deepseek_error}"),
                reason,
                status,
            }),
            other => other.map(direct),
        },
    }
}

/// Whether a failed direct request may be retried through DeepSeek. Orca's
/// own refusals stand, a missing page stays missing, and a private or
/// intranet address is neither reachable from DeepSeek nor its business.
fn should_retry_with_deepseek(error: &FetchError, url: &str) -> bool {
    match error {
        FetchError::Cancelled | FetchError::Rejected(_) => false,
        FetchError::Failed {
            status: Some(404 | 410),
            ..
        } => false,
        FetchError::Failed { .. } => is_public_host(url),
    }
}

/// Host suffixes that only resolve inside a local network.
/// Host suffixes that only resolve inside a local network (RFC 6762,
/// appendix G, and common intranet names).
const INTRANET_SUFFIXES: [&str; 10] = [
    ".local",
    ".localhost",
    ".localdomain",
    ".internal",
    ".intranet",
    ".private",
    ".corp",
    ".home",
    ".lan",
    ".home.arpa",
];

fn is_public_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => is_public_ipv4(ip),
        Some(url::Host::Ipv6(ip)) => match ip.to_ipv4_mapped() {
            Some(mapped) => is_public_ipv4(mapped),
            None => {
                let first = ip.segments()[0];
                let unique_local = first & 0xfe00 == 0xfc00;
                let link_local = first & 0xffc0 == 0xfe80;
                !(ip.is_loopback() || ip.is_unspecified() || unique_local || link_local)
            }
        },
        Some(url::Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            label_count(&domain) >= 2
                && !INTRANET_SUFFIXES
                    .iter()
                    .any(|suffix| domain.ends_with(suffix) || domain == suffix[1..])
        }
        None => false,
    }
}

fn is_public_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    // 0.0.0.0/8 means "this network".
    let this_network = first == 0;
    // 100.64.0.0/10 is the carrier-grade NAT range.
    let shared = first == 100 && second & 0xc0 == 64;
    // 192.0.0.0/24 is reserved for protocol assignments.
    let protocol_assignments = first == 192 && second == 0 && third == 0;
    // 198.18.0.0/15 is for benchmarking, and local proxies hand it out as fake IPs.
    let benchmarking = first == 198 && second & 0xfe == 18;
    // 240.0.0.0/4 is reserved for future use.
    let reserved = first >= 240;
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || this_network
        || shared
        || protocol_assignments
        || benchmarking
        || reserved)
}

/// DeepSeek's Markdown, headed by the page title when the content has no
/// heading of its own.
fn page_text(page: DeepSeekPage) -> String {
    if page.title.trim().is_empty() || page.content.trim_start().starts_with('#') {
        page.content
    } else {
        format!("# {}\n\n{}", page.title.trim(), page.content)
    }
}

/// Upgrade http → https, reject non-HTTP schemes and credentials, require ≥ 2 hostname parts.
fn normalise_url(raw: &str) -> Option<String> {
    let mut parsed = url::Url::parse(raw).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    match parsed.scheme() {
        "https" => {}
        "http" => {
            parsed.set_scheme("https").ok()?;
        }
        _ => return None,
    }
    let host = parsed.host_str()?;
    if label_count(host) < 2 {
        return None;
    }
    Some(parsed.to_string())
}

/// The non-empty dot-separated labels of `host`, so `nas.` counts as one.
fn label_count(host: &str) -> usize {
    host.split('.').filter(|label| !label.is_empty()).count()
}

fn strip_www(h: &str) -> &str {
    h.strip_prefix("www.").unwrap_or(h)
}

/// Same-origin redirect check: allow adding/removing "www." and path changes,
/// disallow cross-origin or cross-protocol hops. Mirrors Claude Code's
/// `isPermittedRedirect`.
fn is_permitted_redirect(original: &str, redirect: &str) -> bool {
    let (Ok(orig), Ok(redir)) = (url::Url::parse(original), url::Url::parse(redirect)) else {
        return false;
    };
    if redir.scheme() != orig.scheme() || redir.port() != orig.port() {
        return false;
    }
    if !redir.username().is_empty() || redir.password().is_some() {
        return false;
    }
    strip_www(redir.host_str().unwrap_or("")) == strip_www(orig.host_str().unwrap_or(""))
}

/// Recursive redirect follower. Uses `Box::pin` to allow async recursion.
async fn fetch_with_redirects(
    client: &reqwest::Client,
    url: &str,
    should_cancel: &dyn Fn() -> bool,
    depth: usize,
) -> Result<String, FetchError> {
    if depth > MAX_REDIRECTS {
        return Err(FetchError::Rejected(format!(
            "too many redirects (exceeded {MAX_REDIRECTS})"
        )));
    }

    let response = await_or_cancel(
        client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/html, text/plain, */*")
            .send(),
        should_cancel,
    )
    .await?
    .map_err(|e| {
        let reason = if e.is_timeout() {
            "timed out"
        } else {
            "the connection failed"
        };
        FetchError::failed(format!("fetch failed for {url}: {e}"), reason)
    })?;

    let status = response.status();

    // Manual redirect: same-origin only.
    if status.is_redirection() {
        if let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        {
            let resolved = url::Url::parse(url)
                .ok()
                .and_then(|base| base.join(location).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| location.to_string());

            return if is_permitted_redirect(url, &resolved) {
                Box::pin(fetch_with_redirects(
                    client,
                    &resolved,
                    should_cancel,
                    depth + 1,
                ))
                .await
            } else {
                Err(FetchError::Rejected(format!(
                    "cross-origin redirect blocked: {resolved}"
                )))
            };
        }
    }

    if !status.is_success() {
        return Err(FetchError::status(
            status.as_u16(),
            format!("HTTP {status} fetching {url}"),
        ));
    }

    let bytes = await_or_cancel(response.bytes(), should_cancel)
        .await?
        .map_err(|e| {
            let reason = if e.is_timeout() {
                "timed out"
            } else {
                "the response could not be read"
            };
            FetchError::failed(format!("failed to read response body: {e}"), reason)
        })?;

    // Enforce body cap before decoding.
    let slice = if bytes.len() > MAX_BODY_BYTES {
        &bytes[..MAX_BODY_BYTES]
    } else {
        &bytes[..]
    };

    Ok(String::from_utf8_lossy(slice).into_owned())
}

fn build_client() -> Result<reqwest::Client, FetchError> {
    let builder = orca_mcp::http::client_builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none()); // we follow redirects manually
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|e| {
        FetchError::failed(
            format!(
                "failed to build HTTP client: {}",
                orca_mcp::http::build_error(&e)
            ),
            "the HTTP client could not start",
        )
    })
}

async fn await_or_cancel<T>(
    future: impl Future<Output = T>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<T, FetchError> {
    tokio::pin!(future);
    loop {
        if should_cancel() {
            return Err(FetchError::Cancelled);
        }
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(CANCEL_POLL) => {}
        }
    }
}

// ── HTML → readable text ───────────────────────────────────────────────────────

/// A page as the direct request read it.
#[derive(Debug)]
struct LocalPage {
    /// The readable text: title, description and body.
    text: String,
    /// Whether the body has text of its own beyond the title and
    /// description. An app shell that renders with JavaScript has none, even
    /// when its `<noscript>` fallback says something.
    readable: bool,
}

#[cfg(test)]
fn html_to_text(body: &str) -> String {
    read_page(body).text
}

/// Convert a response body to readable plain text.
///
/// Strategy (no external HTML crate needed):
/// 1. Extract `<title>` and `<meta name="description">` for a header.
/// 2. Remove `<script>`, `<style>`, `<nav>`, `<footer>`, `<head>` blocks.
/// 3. Replace block-level tags with newlines, strip remaining tags.
/// 4. Decode common HTML entities.
/// 5. Collapse excessive blank lines.
///
/// If the input does not look like HTML it is returned as-is.
fn read_page(body: &str) -> LocalPage {
    if !body.trim_start().starts_with('<') {
        return LocalPage {
            text: body.to_string(),
            readable: !body.trim().is_empty(),
        };
    }

    // -- 1. pull title and meta description -----------------------------------
    let title = extract_tag_inner(body, "title").unwrap_or_default();
    let description = extract_meta_description(body).unwrap_or_default();

    let trimmed = html_body_text(body);
    let without_noscript = remove_tag_block(body, "noscript");
    let readable = if without_noscript.len() == body.len() {
        !trimmed.is_empty()
    } else {
        !html_body_text(&without_noscript).is_empty()
    };

    // -- build final output ---------------------------------------------------
    let mut parts: Vec<String> = Vec::new();
    if !title.is_empty() {
        parts.push(format!("# {}", title.trim()));
    }
    if !description.is_empty() {
        parts.push(description.trim().to_string());
    }
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    LocalPage {
        text: parts.join("\n\n"),
        readable,
    }
}

/// Steps 2–5 of [`read_page`]: the body's text without the head.
fn html_body_text(body: &str) -> String {
    // -- 2. remove noisy blocks -----------------------------------------------
    let mut text = remove_tag_block(body, "script");
    text = remove_tag_block(&text, "style");
    text = remove_tag_block(&text, "nav");
    text = remove_tag_block(&text, "footer");
    text = remove_tag_block(&text, "head");
    text = remove_tag_block(&text, "aside");

    // -- 3. block tags → newlines, inline tags stripped -----------------------
    // Replace common block-level closing/opening tags with a newline first.
    let block_re = Regex::new(
        r"(?i)</?(?:div|p|br|li|ul|ol|h[1-6]|article|section|main|header|blockquote|pre|table|tr|td|th)[^>]*>",
    )
    .expect("valid regex");
    text = block_re.replace_all(&text, "\n").into_owned();

    // Strip remaining tags.
    let tag_re = Regex::new(r"<[^>]+>").expect("valid regex");
    text = tag_re.replace_all(&text, "").into_owned();

    // -- 4. decode common HTML entities ---------------------------------------
    text = decode_entities(&text);

    // -- 5. tidy whitespace ---------------------------------------------------
    // Compress runs of spaces/tabs.
    let space_re = Regex::new(r"[ \t]+").expect("valid regex");
    text = space_re.replace_all(&text, " ").into_owned();

    // Collapse 3+ consecutive newlines to 2.
    let blank_re = Regex::new(r"\n{3,}").expect("valid regex");
    text = blank_re.replace_all(&text, "\n\n").into_owned();

    // Trim each line.
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_tag_inner(html: &str, tag: &str) -> Option<String> {
    let re = Regex::new(&format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>")).ok()?;
    let caps = re.captures(html)?;
    let inner = caps.get(1)?.as_str();
    // Strip any nested tags inside the element.
    let tag_re = Regex::new(r"<[^>]+>").ok()?;
    Some(tag_re.replace_all(inner, "").trim().to_string())
}

fn extract_meta_description(html: &str) -> Option<String> {
    let re = Regex::new(r#"(?i)<meta[^>]+name=["']description["'][^>]+content=["']([^"']+)["']"#)
        .ok()?;
    if let Some(caps) = re.captures(html) {
        return Some(caps.get(1)?.as_str().to_string());
    }
    // Alternative attribute order: content= before name=
    let re2 = Regex::new(r#"(?i)<meta[^>]+content=["']([^"']+)["'][^>]+name=["']description["']"#)
        .ok()?;
    re2.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn remove_tag_block(html: &str, tag: &str) -> String {
    let re = Regex::new(&format!(r"(?is)<{tag}[^>]*>.*?</{tag}>")).expect("valid regex");
    re.replace_all(html, " ").into_owned()
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&copy;", "©")
        .replace("&reg;", "®")
}

// ── prompt keyword filter ──────────────────────────────────────────────────────

/// When `prompt` is provided, return only paragraphs/lines that contain at
/// least one keyword from the prompt. Falls back to the full text when no
/// keyword matches anything (avoids returning an empty result).
fn apply_prompt_filter(text: &str, prompt: Option<&str>) -> String {
    let Some(prompt) = prompt else {
        return text.to_string();
    };
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return text.to_string();
    }

    // Split prompt into lowercase keywords (words ≥ 3 chars, de-duped).
    let mut keywords: Vec<String> = prompt
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3)
        .collect();
    keywords.dedup();

    if keywords.is_empty() {
        return text.to_string();
    }

    // Split text into paragraphs, keep those containing any keyword.
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let matched: Vec<&str> = paragraphs
        .iter()
        .copied()
        .filter(|para| {
            let lower = para.to_lowercase();
            keywords.iter().any(|kw| lower.contains(kw.as_str()))
        })
        .collect();

    if matched.is_empty() {
        text.to_string() // no match: return everything
    } else {
        matched.join("\n\n")
    }
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_url_upgrades_http_to_https() {
        assert_eq!(
            normalise_url("http://example.com/path"),
            Some("https://example.com/path".to_string())
        );
    }

    #[test]
    fn normalise_url_rejects_credentials() {
        assert!(normalise_url("https://user:pass@example.com/").is_none());
    }

    #[test]
    fn normalise_url_rejects_single_label_host() {
        assert!(normalise_url("https://localhost/").is_none());
    }

    #[test]
    fn normalise_url_rejects_single_label_host_with_trailing_dot() {
        assert!(normalise_url("https://nas./").is_none());
        assert!(normalise_url("https://example.com./a").is_some());
    }

    #[test]
    fn normalise_url_rejects_non_http_scheme() {
        assert!(normalise_url("ftp://example.com/file").is_none());
    }

    #[test]
    fn is_permitted_redirect_allows_www_addition() {
        assert!(is_permitted_redirect(
            "https://example.com/a",
            "https://www.example.com/b"
        ));
    }

    #[test]
    fn is_permitted_redirect_blocks_cross_origin() {
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "https://evil.com/b"
        ));
    }

    #[test]
    fn is_permitted_redirect_blocks_protocol_downgrade() {
        assert!(!is_permitted_redirect(
            "https://example.com/a",
            "http://example.com/b"
        ));
    }

    #[test]
    fn html_to_text_extracts_title_and_strips_tags() {
        let html = r#"<html><head><title>Hello World</title></head>
            <body><p>First paragraph.</p><p>Second paragraph.</p></body></html>"#;
        let text = html_to_text(html);
        assert!(
            text.contains("# Hello World"),
            "title should appear as header"
        );
        assert!(text.contains("First paragraph."));
        assert!(text.contains("Second paragraph."));
        assert!(!text.contains('<'), "no raw tags should remain");
    }

    #[test]
    fn html_to_text_removes_script_and_style_blocks() {
        let html = r#"<html><body>
            <script>var x = 1;</script>
            <style>body { color: red; }</style>
            <p>Visible text.</p>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(!text.contains("var x"), "script content should be removed");
        assert!(
            !text.contains("color: red"),
            "style content should be removed"
        );
        assert!(text.contains("Visible text."));
    }

    #[test]
    fn html_to_text_decodes_entities() {
        let html = "<p>Rust &amp; Go &lt;3 &quot;rocks&quot;</p>";
        let text = html_to_text(html);
        assert!(text.contains("Rust & Go <3 \"rocks\""));
    }

    #[test]
    fn html_to_text_passthrough_for_plain_text() {
        let plain = "No HTML here, just plain text.";
        assert_eq!(html_to_text(plain), plain);
    }

    #[test]
    fn apply_prompt_filter_keeps_matching_paragraphs() {
        let text = "Rust is fast.\n\nPython is popular.\n\nRust has ownership.";
        let filtered = apply_prompt_filter(text, Some("rust ownership"));
        assert!(filtered.contains("Rust is fast."));
        assert!(!filtered.contains("Python is popular."));
        assert!(filtered.contains("Rust has ownership."));
    }

    #[test]
    fn apply_prompt_filter_returns_all_when_no_match() {
        let text = "Alpha\n\nBeta\n\nGamma";
        let filtered = apply_prompt_filter(text, Some("zzznotfound"));
        assert_eq!(filtered, text);
    }

    #[test]
    fn apply_prompt_filter_noop_when_prompt_absent() {
        let text = "Some text here.";
        assert_eq!(apply_prompt_filter(text, None), text);
    }

    const PAGE_URL: &str = "https://example.com/a";
    const LOCAL_403: &str = "HTTP 403 Forbidden fetching https://example.com/a";

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test fetch runtime")
            .block_on(future)
    }

    fn page_json(title: &str, content: &str) -> String {
        serde_json::json!({ "title": title, "content": content }).to_string()
    }

    fn failure_message(error: FetchError) -> String {
        match error {
            FetchError::Failed { message, .. } | FetchError::Rejected(message) => message,
            FetchError::Cancelled => "cancelled".to_string(),
        }
    }

    #[test]
    fn retry_decision_skips_rejections_missing_pages_and_private_hosts() {
        for rejected in [
            "cross-origin redirect blocked: https://elsewhere.example/",
            "too many redirects (exceeded 10)",
        ] {
            assert!(
                !should_retry_with_deepseek(&FetchError::Rejected(rejected.into()), PAGE_URL),
                "{rejected}"
            );
        }
        for missing in [404, 410] {
            assert!(
                !should_retry_with_deepseek(&FetchError::status(missing, "gone".into()), PAGE_URL),
                "HTTP {missing}"
            );
        }
        for host in [
            "10.0.0.5",
            "192.168.1.10",
            "172.16.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "0.1.2.3",
            "100.64.0.1",
            "192.0.0.8",
            "198.18.0.5",
            "224.0.0.1",
            "240.0.0.1",
            "[fd00::1]",
            "[::1]",
            "printer.local",
            "svc.internal",
            "nas.lan",
            "router.home.arpa",
            "box.localdomain",
            "files.corp",
            "nas.home",
            "wiki.intranet",
            "app.private",
            "nas.",
        ] {
            assert!(
                !should_retry_with_deepseek(
                    &FetchError::status(503, "down".into()),
                    &format!("https://{host}/x")
                ),
                "{host}"
            );
        }
        for status in [403, 429, 503] {
            assert!(
                should_retry_with_deepseek(&FetchError::status(status, "no".into()), PAGE_URL),
                "HTTP {status}"
            );
        }
        assert!(should_retry_with_deepseek(
            &FetchError::failed("timed out fetching https://example.com/a", "timed out"),
            PAGE_URL
        ));
    }

    #[test]
    fn empty_extraction_is_retried() {
        let fixture = crate::test_http::serve(vec![(200, page_json("A", "# A\n\nbody"))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let fetched = block_on(finish_with_deepseek(
            PAGE_URL,
            Ok(LocalPage {
                text: "# App".to_string(),
                readable: false,
            }),
            Some(&access),
            &|| false,
        ))
        .expect("DeepSeek answers");

        assert_eq!(
            fetched.note.as_deref(),
            Some(
                "[The direct request failed (no readable text), so this page was fetched through the DeepSeek Fetch API.]"
            )
        );
        assert_eq!(fetched.text, "# A\n\nbody");
        assert_eq!(fixture.requests().len(), 1);
    }

    #[test]
    fn app_shell_page_has_no_readable_body() {
        let shell = r#"<!doctype html><html><head><title>App</title><meta name="description" content="An app"></head><body><div id="root"></div><noscript>You need to enable JavaScript to run this app.</noscript><script>boot()</script></body></html>"#;

        let page = read_page(shell);

        assert!(!page.readable, "{}", page.text);
        assert!(page.text.starts_with("# App"), "{}", page.text);
        assert!(
            read_page("<html><head><title>Post</title></head><body><p>Real words.</p><noscript>Enable JS</noscript></body></html>")
                .readable
        );
        assert!(read_page("plain text").readable);
        assert!(!read_page("  \n").readable);
    }

    fn untouched_deepseek() -> (crate::test_http::Untouched, DeepSeekWebAccess) {
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");
        (deepseek, access)
    }

    #[test]
    fn cross_origin_redirect_is_rejected_and_never_sent_to_deepseek() {
        let site = crate::test_http::serve_replies(vec![crate::test_http::Reply::redirect(
            302,
            "http://elsewhere.example/landing",
        )]);
        let local = block_on(fetch_or_cancel(&format!("{}/a", site.origin), &|| false));
        assert!(
            matches!(&local, Err(FetchError::Rejected(message)) if message.starts_with("cross-origin redirect blocked")),
            "{local:?}"
        );
        let (deepseek, access) = untouched_deepseek();

        let outcome = block_on(finish_with_deepseek(
            PAGE_URL,
            local,
            Some(&access),
            &|| false,
        ));

        assert!(
            matches!(outcome, Err(FetchError::Rejected(_))),
            "{outcome:?}"
        );
        deepseek.assert_untouched("DeepSeek");
        assert_eq!(site.requests().len(), 1);
    }

    #[test]
    fn redirect_loop_is_rejected_and_never_sent_to_deepseek() {
        let site = crate::test_http::serve_replies(
            (0..=MAX_REDIRECTS)
                .map(|_| crate::test_http::Reply::redirect(302, "/loop"))
                .collect(),
        );
        let local = block_on(fetch_or_cancel(&format!("{}/loop", site.origin), &|| false));
        assert!(
            matches!(&local, Err(FetchError::Rejected(message)) if message.starts_with("too many redirects")),
            "{local:?}"
        );
        let (deepseek, access) = untouched_deepseek();

        let outcome = block_on(finish_with_deepseek(
            PAGE_URL,
            local,
            Some(&access),
            &|| false,
        ));

        assert!(
            matches!(outcome, Err(FetchError::Rejected(_))),
            "{outcome:?}"
        );
        deepseek.assert_untouched("DeepSeek");
        assert_eq!(site.requests().len(), MAX_REDIRECTS + 1);
    }

    #[test]
    fn direct_requests_identify_orca() {
        let site = crate::test_http::serve(vec![(200, "plain body".into())]);

        let page = block_on(fetch_or_cancel(&format!("{}/a", site.origin), &|| false))
            .expect("the fixture answers");

        assert_eq!(page.text, "plain body");
        let requests = site.requests();
        let expected = format!("\r\nuser-agent: {USER_AGENT}\r\n").to_ascii_lowercase();
        assert!(
            requests[0].to_ascii_lowercase().contains(&expected),
            "{}",
            requests[0]
        );
    }

    #[test]
    fn missing_page_is_never_sent_to_deepseek() {
        let site = crate::test_http::serve(vec![(404, "missing".into())]);
        let local = block_on(fetch_or_cancel(&format!("{}/gone", site.origin), &|| false));
        assert!(
            matches!(
                &local,
                Err(FetchError::Failed {
                    status: Some(404),
                    ..
                })
            ),
            "{local:?}"
        );
        let (deepseek, access) = untouched_deepseek();

        let outcome = block_on(finish_with_deepseek(
            PAGE_URL,
            local,
            Some(&access),
            &|| false,
        ));

        assert!(
            matches!(
                outcome,
                Err(FetchError::Failed {
                    status: Some(404),
                    ..
                })
            ),
            "{outcome:?}"
        );
        deepseek.assert_untouched("DeepSeek");
        site.requests();
    }

    #[test]
    fn fallback_output_starts_with_note_and_applies_prompt_filter() {
        let fixture = crate::test_http::serve(vec![(
            200,
            page_json("A", "# A\n\nRust has ownership.\n\nPython is popular."),
        )]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let fetched = block_on(finish_with_deepseek(
            PAGE_URL,
            Err(FetchError::status(403, LOCAL_403.into())),
            Some(&access),
            &|| false,
        ))
        .expect("DeepSeek answers");
        let output = render(fetched, Some("rust ownership"));

        assert!(
            output.starts_with(
                "[The direct request failed (HTTP 403), so this page was fetched through the DeepSeek Fetch API.]\n\n"
            ),
            "{output}"
        );
        assert!(output.contains("Rust has ownership."), "{output}");
        assert!(!output.contains("Python is popular."), "{output}");
        fixture.requests();
    }

    #[test]
    fn both_failures_are_reported_together() {
        let fixture =
            crate::test_http::serve(vec![(500, r#"{"error":{"message":"boom"}}"#.into())]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let error = block_on(finish_with_deepseek(
            PAGE_URL,
            Err(FetchError::status(403, LOCAL_403.into())),
            Some(&access),
            &|| false,
        ))
        .expect_err("both requests fail");

        assert_eq!(
            failure_message(error),
            format!("{LOCAL_403}; DeepSeek Fetch also failed: DeepSeek returned HTTP 500: boom")
        );
        fixture.requests();
    }

    #[test]
    fn without_deepseek_access_errors_are_unchanged() {
        let error = block_on(finish_with_deepseek(
            PAGE_URL,
            Err(FetchError::status(403, LOCAL_403.into())),
            None,
            &|| false,
        ))
        .expect_err("nothing retries");
        assert_eq!(failure_message(error), LOCAL_403);

        let blank = block_on(finish_with_deepseek(
            PAGE_URL,
            Ok(LocalPage {
                text: "# App".to_string(),
                readable: false,
            }),
            None,
            &|| false,
        ))
        .expect("a page without readable text is returned as before");
        assert_eq!(blank.note, None);
        assert_eq!(blank.text, "# App");
    }
}
