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

// ── public entry points ────────────────────────────────────────────────────────

pub fn execute(request: &ToolRequest, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, max_bytes, || false)
}

pub fn execute_or_cancel(
    request: &ToolRequest,
    max_bytes: usize,
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

    let outcome = rt.block_on(fetch_or_cancel(&args, &should_cancel));

    match outcome {
        Ok(text) => {
            let text = apply_prompt_filter(&text, args.prompt.as_deref());
            let effective_max = max_bytes.min(MAX_OUTPUT_CHARS);
            let (output, truncated) = truncate_output(text, effective_max);
            ToolResult::completed(request, output, truncated)
        }
        Err(FetchError::Cancelled) => {
            ToolResult::cancelled(request, "web fetch was cancelled", None)
        }
        Err(FetchError::Failed(msg)) => ToolResult::failed(request, msg, None),
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
    Failed(String),
}

impl From<String> for FetchError {
    fn from(s: String) -> Self {
        FetchError::Failed(s)
    }
}

async fn fetch_or_cancel(
    args: &FetchArgs,
    should_cancel: &dyn Fn() -> bool,
) -> Result<String, FetchError> {
    let url = normalise_url(&args.url)
        .ok_or_else(|| FetchError::Failed(format!("invalid or unsupported URL: {}", args.url)))?;

    let client = build_client()?;
    let body = fetch_with_redirects(&client, &url, should_cancel, 0).await?;
    Ok(html_to_text(&body))
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
    if host.split('.').count() < 2 {
        return None;
    }
    Some(parsed.to_string())
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
        return Err(FetchError::Failed(format!(
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
    .map_err(|e| FetchError::Failed(format!("fetch failed for {url}: {e}")))?;

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
                Err(FetchError::Failed(format!(
                    "cross-origin redirect blocked: {resolved}"
                )))
            };
        }
    }

    if !status.is_success() {
        return Err(FetchError::Failed(format!("HTTP {status} fetching {url}")));
    }

    let bytes = await_or_cancel(response.bytes(), should_cancel)
        .await?
        .map_err(|e| FetchError::Failed(format!("failed to read response body: {e}")))?;

    // Enforce body cap before decoding.
    let slice = if bytes.len() > MAX_BODY_BYTES {
        &bytes[..MAX_BODY_BYTES]
    } else {
        &bytes[..]
    };

    Ok(String::from_utf8_lossy(slice).into_owned())
}

fn build_client() -> Result<reqwest::Client, FetchError> {
    orca_mcp::http::client_builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none()) // we follow redirects manually
        .build()
        .map_err(|e| {
            FetchError::Failed(format!(
                "failed to build HTTP client: {}",
                orca_mcp::http::build_error(&e)
            ))
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

/// Convert an HTML body to readable plain text.
///
/// Strategy (no external HTML crate needed):
/// 1. Extract `<title>` and `<meta name="description">` for a header.
/// 2. Remove `<script>`, `<style>`, `<nav>`, `<footer>`, `<head>` blocks.
/// 3. Replace block-level tags with newlines, strip remaining tags.
/// 4. Decode common HTML entities.
/// 5. Collapse excessive blank lines.
///
/// If the input does not look like HTML it is returned as-is.
fn html_to_text(body: &str) -> String {
    if !body.trim_start().starts_with('<') {
        return body.to_string();
    }

    // -- 1. pull title and meta description -----------------------------------
    let title = extract_tag_inner(body, "title").unwrap_or_default();
    let description = extract_meta_description(body).unwrap_or_default();

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
    let trimmed: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

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
    parts.join("\n\n")
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
}
