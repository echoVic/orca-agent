use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use reqwest::blocking::{
    Client as BlockingClient, RequestBuilder as BlockingRequestBuilder,
    Response as BlockingResponse,
};
use reqwest::{Client, RequestBuilder, Response};

const CONNECT_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;
const STREAMING_IDLE_READ_TIMEOUT_SECS: u64 = 300;
pub(crate) const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60_000;
const BACKOFF_FACTOR: f64 = 2.0;
const JITTER_FACTOR: f64 = 0.1;

/// Counts transport sends across HTTP, response-recovery and runtime retries.
/// Clone one budget for retries of the same logical call; independent child or
/// summary operations get their own budget and share only the quota cooldown.
#[derive(Clone, Debug)]
pub struct RetryBudget {
    state: Arc<Mutex<RetryBudgetState>>,
    max_attempts: u32,
    max_wait: Duration,
    deadline: Option<Instant>,
}

#[derive(Debug, Default)]
struct RetryBudgetState {
    attempts: u32,
    waited: Duration,
    reserved_wait: Duration,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(120), None)
    }
}

impl RetryBudget {
    pub fn new(max_attempts: u32, max_wait: Duration, deadline: Option<Instant>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RetryBudgetState::default())),
            max_attempts,
            max_wait,
            deadline,
        }
    }

    pub fn attempts(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attempts
    }

    pub fn can_retry(&self) -> bool {
        self.attempts() < self.max_attempts && self.remaining_time().is_ok()
    }

    fn remaining_time(&self) -> Result<Option<Duration>, String> {
        match self.deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err("provider retry deadline exceeded".into())
                } else {
                    Ok(Some(remaining))
                }
            }
            None => Ok(None),
        }
    }

    fn charge_send(&self) -> Result<(), String> {
        self.remaining_time()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.attempts >= self.max_attempts {
            return Err("provider logical retry send budget exhausted".into());
        }
        state.attempts += 1;
        Ok(())
    }

    fn reserve_wait(&self, delay: Duration) -> Result<(), String> {
        if self
            .remaining_time()?
            .is_some_and(|remaining| delay >= remaining)
        {
            return Err("provider Retry-After/backoff exceeds retry deadline".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if delay
            > self
                .max_wait
                .saturating_sub(state.waited.saturating_add(state.reserved_wait))
        {
            return Err("provider logical retry wait budget exhausted".into());
        }
        state.reserved_wait = state.reserved_wait.saturating_add(delay);
        Ok(())
    }

    fn record_wait(&self, started: Instant, reserved: Duration) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.reserved_wait = state.reserved_wait.saturating_sub(reserved);
        state.waited = state.waited.saturating_add(started.elapsed());
    }

    pub fn wait_blocking(&self, delay: Duration, cancel: &CancelToken) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("request cancelled".into());
        }
        self.reserve_wait(delay)?;
        let reservation = WaitReservation {
            budget: self,
            started: Instant::now(),
            delay,
        };
        let until = reservation
            .started
            .checked_add(delay)
            .ok_or("provider retry wait out of range")?;
        while Instant::now() < until {
            if cancel.is_cancelled() {
                return Err("request cancelled".into());
            }
            thread::sleep(
                until
                    .saturating_duration_since(Instant::now())
                    .min(CANCELLATION_POLL_INTERVAL),
            );
        }
        drop(reservation);
        self.remaining_time()?;
        Ok(())
    }

    async fn wait_async(&self, delay: Duration, cancel: &CancelToken) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("request cancelled".into());
        }
        self.reserve_wait(delay)?;
        let reservation = WaitReservation {
            budget: self,
            started: Instant::now(),
            delay,
        };
        let result = sleep_with_cancel(delay, cancel).await;
        drop(reservation);
        result?;
        self.remaining_time()?;
        Ok(())
    }
}

struct WaitReservation<'a> {
    budget: &'a RetryBudget,
    started: Instant,
    delay: Duration,
}
impl Drop for WaitReservation<'_> {
    fn drop(&mut self) {
        self.budget.record_wait(self.started, self.delay);
    }
}

// Only irreversible digests are retained. Neither URLs with query data nor
// plaintext authentication values are stored or logged by the quota gate.
#[derive(Default)]
struct Cooldown {
    until: Option<Instant>,
    next_admission: Option<Instant>,
    out_of_range: bool,
}
static COOLDOWNS: LazyLock<Mutex<HashMap<[u8; 32], Cooldown>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const COOLDOWN_ADMISSION_SPACING: Duration = Duration::from_millis(25);

fn quota_key(url: &reqwest::Url, headers: &reqwest::header::HeaderMap) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(url.origin().ascii_serialization());
    hash.update(url.path().trim_end_matches('/'));
    for name in ["authorization", "x-api-key"] {
        hash.update([0]);
        if let Some(value) = headers.get(name) {
            hash.update(value.as_bytes());
        }
    }
    hash.finalize().into()
}

fn extend_cooldown(key: [u8; 32], headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let delay = retry_after_header(headers)?;
    let until = Instant::now().checked_add(delay);
    let mut cooldowns = COOLDOWNS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cooldowns.entry(key).or_default();
    match until {
        Some(until) => {
            entry.until = Some(entry.until.map_or(until, |previous| previous.max(until)))
        }
        None => entry.out_of_range = true,
    }
    Some(delay)
}

// Space admission after a cooldown. Callers recheck after each wait:
// another 429/503 can extend the shared deadline while a slot is pending.
fn cooldown_delay(key: [u8; 32]) -> Duration {
    let now = Instant::now();
    let mut cooldowns = COOLDOWNS.lock().unwrap_or_else(|e| e.into_inner());
    // Keep newly expired entries until admission has actually occurred. A
    // short quiet retention bounds the map without releasing queued waiters.
    cooldowns.retain(|_, entry| {
        entry.out_of_range
            || entry.until.is_some_and(|until| {
                until > now
                    || entry.next_admission.is_none()
                    || entry.next_admission.is_some_and(|next| {
                        now.saturating_duration_since(next) < Duration::from_secs(60)
                    })
            })
    });
    let Some(entry) = cooldowns.get_mut(&key) else {
        return Duration::ZERO;
    };
    if entry.out_of_range {
        return Duration::MAX;
    }
    let admission = entry
        .until
        .unwrap_or(now)
        .max(entry.next_admission.unwrap_or(now));
    if admission > now {
        return admission.duration_since(now);
    }
    // Allocate only an immediately usable admission under the mutex. Late
    // wakeups recheck instead of bunching preallocated, expired slots.
    entry.next_admission = now.checked_add(COOLDOWN_ADMISSION_SPACING);
    Duration::ZERO
}

async fn admit_async(
    key: [u8; 32],
    budget: &RetryBudget,
    cancel: &CancelToken,
) -> Result<(), String> {
    loop {
        if cancel.is_cancelled() {
            return Err("request cancelled".into());
        }
        let delay = cooldown_delay(key);
        if delay.is_zero() {
            return budget.charge_send();
        }
        budget.wait_async(delay, cancel).await?;
    }
}

fn admit_blocking(key: [u8; 32], budget: &RetryBudget, cancel: &CancelToken) -> Result<(), String> {
    loop {
        if cancel.is_cancelled() {
            return Err("request cancelled".into());
        }
        let delay = cooldown_delay(key);
        if delay.is_zero() {
            return budget.charge_send();
        }
        budget.wait_blocking(delay, cancel)?;
    }
}

/// 429 (rate limit) and every server-side 5xx are transient. Providers and the CDNs in front
/// of them emit codes no allow-list can enumerate — Cloudflare alone uses 520-526 and 598 —
/// and a 5xx that is *not* transient simply fails the retry loop with the last response.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// The blocking client, built on first use. A system that cannot build one
/// (no CA certificates) keeps the reason, which every caller gets.
static CLIENT: LazyLock<Result<BlockingClient, String>> = LazyLock::new(|| {
    orca_mcp::http::blocking_client_builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|error| {
            format!(
                "failed to build HTTP client: {}",
                orca_mcp::http::build_error(&error)
            )
        })
});

pub(crate) fn streaming_client() -> Result<Client, String> {
    orca_mcp::http::client_builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .hickory_dns(true)
        .build()
        .map_err(|error| {
            format!(
                "failed to build streaming HTTP client: {}",
                orca_mcp::http::build_error(&error)
            )
        })
}

pub(crate) fn streaming_idle_read_timeout() -> Duration {
    Duration::from_secs(STREAMING_IDLE_READ_TIMEOUT_SECS)
}

pub fn client() -> Result<&'static BlockingClient, String> {
    CLIENT.as_ref().map_err(Clone::clone)
}

pub fn execute_with_retry(
    build_request: impl Fn(&BlockingClient) -> BlockingRequestBuilder,
) -> Result<BlockingResponse, String> {
    execute_with_retry_budget(build_request, &RetryBudget::default(), &CancelToken::new())
}

pub fn execute_with_retry_budget(
    build_request: impl Fn(&BlockingClient) -> BlockingRequestBuilder,
    budget: &RetryBudget,
    cancel: &CancelToken,
) -> Result<BlockingResponse, String> {
    let client = client()?;
    let mut attempt: u32 = 0;
    loop {
        let request = build_request(client)
            .build()
            .map_err(|e| format!("request build failed: {e}"))?;
        let (result, key) = send_built_blocking_request(client, request, cancel, budget)?;
        match result {
            Ok(resp) => {
                let status = resp.status();
                if !is_retryable_status(status.as_u16()) {
                    return resp
                        .error_for_status()
                        .map_err(|e| format!("request error: {e}"));
                }
                let retry_after = extend_cooldown(key, resp.headers());
                if attempt >= MAX_RETRIES || !budget.can_retry() {
                    return Err(format!("max retries exceeded (last status: {status})"));
                }
                // The next admission enforces Retry-After for this and peer calls.
                if retry_after.is_none() {
                    budget.wait_blocking(compute_backoff(attempt), cancel)?;
                }
            }
            Err(err) => {
                if attempt >= MAX_RETRIES || !budget.can_retry() || !is_retryable_error(&err) {
                    return Err(format!(
                        "request failed after {} attempts: {err}",
                        attempt + 1
                    ));
                }
                budget.wait_blocking(compute_backoff(attempt), cancel)?;
            }
        }
        attempt += 1;
    }
}

pub async fn execute_streaming_with_retry(
    client: &Client,
    build_request: impl Fn(&Client) -> RequestBuilder,
    cancel: &CancelToken,
) -> Result<Response, String> {
    execute_streaming_with_retry_budget(client, build_request, cancel, &RetryBudget::default())
        .await
}

pub async fn execute_streaming_with_retry_budget(
    client: &Client,
    build_request: impl Fn(&Client) -> RequestBuilder,
    cancel: &CancelToken,
    budget: &RetryBudget,
) -> Result<Response, String> {
    let mut attempt: u32 = 0;
    loop {
        let request = build_request(client)
            .build()
            .map_err(|e| format!("request build failed: {e}"))?;
        let (result, key) = send_built_streaming_request(client, request, cancel, budget).await?;
        match result {
            Ok(resp) => {
                let status = resp.status();
                if !is_retryable_status(status.as_u16()) {
                    if status.is_client_error() || status.is_server_error() {
                        let body = response_text_with_cancel(resp, cancel).await?;
                        return Err(format!("request error ({status}): {body}"));
                    }
                    return Ok(resp);
                }
                let retry_after = extend_cooldown(key, resp.headers());
                if attempt >= MAX_RETRIES || !budget.can_retry() {
                    return Err(format!("max retries exceeded (last status: {status})"));
                }
                if retry_after.is_none() {
                    budget.wait_async(compute_backoff(attempt), cancel).await?;
                }
            }
            Err(err) => {
                if attempt >= MAX_RETRIES || !budget.can_retry() || !is_retryable_error(&err) {
                    return Err(format!(
                        "request failed after {} attempts: {err}",
                        attempt + 1
                    ));
                }
                budget.wait_async(compute_backoff(attempt), cancel).await?;
            }
        }
        attempt += 1;
    }
}

#[cfg(test)]
pub(crate) async fn execute_streaming_once(
    request: RequestBuilder,
    cancel: &CancelToken,
) -> Result<Response, String> {
    execute_streaming_once_with_budget(
        request,
        cancel,
        &RetryBudget::new(
            1,
            Duration::from_secs(120),
            Some(Instant::now() + Duration::from_secs(REQUEST_TIMEOUT_SECS)),
        ),
    )
    .await
}

pub(crate) async fn execute_streaming_once_with_budget(
    request: RequestBuilder,
    cancel: &CancelToken,
    budget: &RetryBudget,
) -> Result<Response, String> {
    let (client, request) = request
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build_split();
    let request = request.map_err(|e| format!("request build failed: {e}"))?;
    let (result, key) = send_built_streaming_request(&client, request, cancel, budget).await?;
    let response = result.map_err(|error| format!("request failed after 1 attempt: {error}"))?;
    let status = response.status();
    if is_retryable_status(status.as_u16()) {
        extend_cooldown(key, response.headers());
    }
    if status.is_client_error() || status.is_server_error() {
        if std::env::var_os("ORCA_SUMMARY_DEBUG").is_some() {
            eprintln!(
                "orca.remote_summary_http_failure status={}",
                status.as_u16()
            );
        }
        return Err(format!("request error ({status})"));
    }
    Ok(response)
}

// Redirects are explicit so every transport hop uses the same admission gate
// and actual-send budget. The returned key belongs to the final destination,
// including its remaining credentials, so Retry-After cools the correct quota.
fn redirect_target(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    previous: &reqwest::Url,
) -> Result<Option<reqwest::Url>, String> {
    if !status.is_redirection() {
        return Ok(None);
    }
    if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Err("unsupported provider redirect status".into());
    }
    let location = headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or("provider redirect missing valid Location")?;
    let next = previous
        .join(location)
        .map_err(|_| "provider redirect has invalid Location")?;
    if !matches!(next.scheme(), "http" | "https")
        || !next.username().is_empty()
        || next.password().is_some()
    {
        return Err("provider redirect has unsupported target".into());
    }
    if previous.scheme() == "https" && next.scheme() == "http" {
        return Err("provider redirect would downgrade HTTPS to plaintext".into());
    }
    Ok(Some(next))
}

fn prepare_redirect(
    status: reqwest::StatusCode,
    next: &reqwest::Url,
    previous: &reqwest::Url,
    method: &mut reqwest::Method,
    headers: &mut reqwest::header::HeaderMap,
) -> bool {
    let drop_body = status.as_u16() == 303
        || (matches!(status.as_u16(), 301 | 302) && *method == reqwest::Method::POST);
    if drop_body {
        if *method != reqwest::Method::HEAD {
            *method = reqwest::Method::GET;
        }
        for name in [
            "content-type",
            "content-length",
            "content-encoding",
            "transfer-encoding",
        ] {
            headers.remove(name);
        }
    }
    headers.remove(reqwest::header::HOST);
    if next.origin() != previous.origin() {
        let sensitive: Vec<_> = headers
            .iter()
            .filter(|(_, value)| value.is_sensitive())
            .map(|(name, _)| name.clone())
            .collect();
        for name in sensitive {
            headers.remove(name);
        }
        for name in [
            "authorization",
            "cookie",
            "cookie2",
            "proxy-authorization",
            "www-authenticate",
            "x-api-key",
            "api-key",
            "referer",
        ] {
            headers.remove(name);
        }
    }
    drop_body
}

fn request_retry_budget(
    budget: &RetryBudget,
    timeout: Option<Duration>,
) -> Result<RetryBudget, String> {
    let mut scoped = budget.clone();
    if let Some(timeout) = timeout {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("provider request timeout out of range")?;
        scoped.deadline = Some(
            scoped
                .deadline
                .map_or(deadline, |existing| existing.min(deadline)),
        );
    }
    Ok(scoped)
}

type HttpSendResult<T> = Result<(Result<T, reqwest::Error>, [u8; 32]), String>;

fn send_built_blocking_request(
    client: &BlockingClient,
    mut request: reqwest::blocking::Request,
    cancel: &CancelToken,
    budget: &RetryBudget,
) -> HttpSendResult<BlockingResponse> {
    let scoped = request_retry_budget(
        budget,
        Some(
            request
                .timeout()
                .copied()
                .unwrap_or(Duration::from_secs(REQUEST_TIMEOUT_SECS)),
        ),
    )?;
    let budget = &scoped;
    loop {
        let key = quota_key(request.url(), request.headers());
        admit_blocking(key, budget, cancel)?;
        *request.timeout_mut() = budget.remaining_time()?;
        let replay = request.try_clone();
        match client.execute(request) {
            Ok(response) => {
                let Some(next) =
                    redirect_target(response.status(), response.headers(), response.url())?
                else {
                    return Ok((Ok(response), key));
                };
                let mut follow = replay.ok_or("provider redirect body cannot be replayed")?;
                let mut method = follow.method().clone();
                let previous = follow.url().clone();
                if prepare_redirect(
                    response.status(),
                    &next,
                    &previous,
                    &mut method,
                    follow.headers_mut(),
                ) {
                    *follow.body_mut() = None;
                }
                *follow.method_mut() = method;
                *follow.url_mut() = next;
                request = follow;
            }
            Err(error) => return Ok((Err(error), key)),
        }
    }
}

async fn send_built_streaming_request(
    client: &Client,
    mut request: reqwest::Request,
    cancel: &CancelToken,
    budget: &RetryBudget,
) -> HttpSendResult<Response> {
    let scoped = request_retry_budget(budget, request.timeout().copied())?;
    let budget = &scoped;
    loop {
        let key = quota_key(request.url(), request.headers());
        admit_async(key, budget, cancel).await?;
        *request.timeout_mut() = budget.remaining_time()?;
        let replay = request.try_clone();
        let result = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => return Err("request cancelled".into()),
            result = client.execute(request) => result,
        };
        match result {
            Ok(response) => {
                let Some(next) =
                    redirect_target(response.status(), response.headers(), response.url())?
                else {
                    return Ok((Ok(response), key));
                };
                let mut follow = replay.ok_or("provider redirect body cannot be replayed")?;
                let mut method = follow.method().clone();
                let previous = follow.url().clone();
                if prepare_redirect(
                    response.status(),
                    &next,
                    &previous,
                    &mut method,
                    follow.headers_mut(),
                ) {
                    *follow.body_mut() = None;
                }
                *follow.method_mut() = method;
                *follow.url_mut() = next;
                request = follow;
            }
            Err(error) => return Ok((Err(error), key)),
        }
    }
}

async fn response_text_with_cancel(
    response: Response,
    cancel: &CancelToken,
) -> Result<String, String> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err("request cancelled".to_string()),
        result = tokio::time::timeout(streaming_idle_read_timeout(), response.text()) => {
            match result {
                Ok(Ok(body)) => Ok(body),
                Ok(Err(error)) => Err(format!("response body read failed: {error}")),
                Err(_) => Err(format!(
                    "response body idle read timed out after {:?}",
                    streaming_idle_read_timeout()
                )),
            }
        }
    }
}

async fn sleep_with_cancel(delay: Duration, cancel: &CancelToken) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => Err("request cancelled".to_string()),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

pub(crate) async fn wait_for_cancel(cancel: &CancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
    }
}

fn compute_backoff(attempt: u32) -> Duration {
    let base_ms = INITIAL_BACKOFF_MS as f64 * BACKOFF_FACTOR.powi(attempt as i32);
    let capped_ms = base_ms.min(MAX_BACKOFF_MS as f64);
    let jitter = 1.0 + (jitter_value() * 2.0 - 1.0) * JITTER_FACTOR;
    Duration::from_millis((capped_ms * jitter) as u64)
}

fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let now = chrono::Utc::now();
    let date = chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&chrono::Utc))
        .or_else(|| parse_rfc850_http_date(value, now))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
                .ok()
                .map(|date| date.and_utc())
        })?;
    Some((date - now).to_std().unwrap_or(Duration::ZERO))
}

fn parse_rfc850_http_date(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::Datelike;
    use chrono::format::{Parsed, StrftimeItems, parse};

    let mut parsed = Parsed::new();
    parse(
        &mut parsed,
        value,
        StrftimeItems::new("%A, %d-%b-%y %H:%M:%S GMT"),
    )
    .ok()?;
    // HTTP's two-digit year has a sliding 50-year future window. Resolve
    // the century before validating the weekday; chrono's fixed %y pivot
    // would otherwise interpret a valid 2071 date as 1971.
    let cutoff = now.checked_add_months(chrono::Months::new(600))?;
    let mut year = cutoff.year().div_euclid(100) * 100 + parsed.year_mod_100()?;
    if year > cutoff.year()
        || (year == cutoff.year()
            && (parsed.month()?, parsed.day()?, parsed.to_naive_time().ok()?)
                > (cutoff.month(), cutoff.day(), cutoff.time()))
    {
        year -= 100;
    }
    parsed.set_year(i64::from(year)).ok()?;
    parsed
        .to_naive_datetime_with_offset(0)
        .ok()
        .map(|date| date.and_utc())
}

fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

fn jitter_value() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos as f64) / (u32::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Instant;

    #[test]
    fn redirect_review_rejects_https_downgrades_and_preserves_secure_targets() {
        let secure = reqwest::Url::parse("https://provider.invalid/start").unwrap();
        for status in [301, 302, 303, 307, 308] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::LOCATION,
                "http://provider.invalid/next".parse().unwrap(),
            );
            let result = redirect_target(
                reqwest::StatusCode::from_u16(status).unwrap(),
                &headers,
                &secure,
            );
            assert!(
                result.is_err(),
                "HTTPS request must reject a plaintext redirect"
            );
            headers.insert(
                reqwest::header::LOCATION,
                "https://destination.invalid/next".parse().unwrap(),
            );
            assert!(
                redirect_target(
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    &headers,
                    &secure
                )
                .unwrap()
                .is_some()
            );
        }
        let local = reqwest::Url::parse("http://127.0.0.1/start").unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::LOCATION, "/next".parse().unwrap());
        assert!(
            redirect_target(reqwest::StatusCode::TEMPORARY_REDIRECT, &headers, &local)
                .unwrap()
                .is_some()
        );
    }

    fn redirect_server(
        status: u16,
        location: &str,
        max_requests: usize,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/start", listener.local_addr().unwrap());
        let location = location.to_string();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut requests = Vec::new();
            while requests.len() < max_requests && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let size = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + size {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let final_hop =
                    request.starts_with("POST /final ") || request.starts_with("GET /final ");
                requests.push(request);
                if final_hop {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                } else {
                    write!(stream, "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            }
            requests
        });
        (url, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_streaming_preserves_post_or_switches_to_get_and_charges_hops() {
        let client = streaming_client().unwrap();
        for status in [301, 302, 303, 307, 308] {
            let (url, server) = redirect_server(status, "/final", 2);
            let budget = RetryBudget::new(5, Duration::from_secs(1), None);
            let response = execute_streaming_with_retry_budget(
                &client,
                |c| c.post(&url).bearer_auth("same-origin").body("payload"),
                &CancelToken::new(),
                &budget,
            )
            .await
            .unwrap();
            assert_eq!(response.text().await.unwrap(), "ok", "status {status}");
            assert_eq!(budget.attempts(), 2);
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests[1]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer same-origin")
            );
            if status == 307 || status == 308 {
                assert!(requests[1].starts_with("POST /final "));
                assert!(requests[1].ends_with("payload"));
            } else {
                assert!(requests[1].starts_with("GET /final "));
                assert!(!requests[1].contains("content-length: 7"));
                assert!(!requests[1].ends_with("payload"));
            }
        }
    }

    #[test]
    fn redirect_review_blocking_preserves_post_and_counts_every_hop() {
        let (url, server) = redirect_server(308, "/final", 2);
        let budget = RetryBudget::new(5, Duration::from_secs(1), None);
        let response = execute_with_retry_budget(
            |c| c.post(&url).body("payload"),
            &budget,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(response.text().unwrap(), "ok");
        assert_eq!(budget.attempts(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /final "));
        assert!(requests[1].ends_with("payload"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_loop_cannot_bypass_actual_send_cap() {
        let (url, server) = redirect_server(307, "/start", 3);
        let client = streaming_client().unwrap();
        let budget = RetryBudget::new(3, Duration::from_secs(1), None);
        let error = execute_streaming_with_retry_budget(
            &client,
            |c| c.post(&url).body("payload"),
            &CancelToken::new(),
            &budget,
        )
        .await
        .unwrap_err();
        assert!(error.contains("send budget"), "{error}");
        assert_eq!(budget.attempts(), 3);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_cross_origin_drops_credentials() {
        let (destination, target) = redirect_server(307, "/final", 1);
        let final_url = destination.replace("/start", "/final");
        let (url, source) = redirect_server(307, &final_url, 1);
        let client = streaming_client().unwrap();
        let budget = RetryBudget::new(5, Duration::from_secs(1), None);
        let response = execute_streaming_with_retry_budget(
            &client,
            |c| {
                c.post(&url)
                    .bearer_auth("secret")
                    .header("cookie", "secret-cookie")
                    .header("x-api-key", "secret-key")
                    .body("payload")
            },
            &CancelToken::new(),
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        source.join().unwrap();
        let requests = target.join().unwrap();
        let request = requests[0].to_ascii_lowercase();
        assert!(!request.contains("secret"), "{request}");
        assert_eq!(budget.attempts(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_summary_once_follows_redirect_with_shared_budget() {
        let (url, server) = redirect_server(307, "/final", 2);
        let client = streaming_client().unwrap();
        let budget = RetryBudget::new(2, Duration::from_secs(1), None);
        let response = execute_streaming_once_with_budget(
            client.post(&url).body("payload"),
            &CancelToken::new(),
            &budget,
        )
        .await
        .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(budget.attempts(), 2);
        assert!(server.join().unwrap()[1].ends_with("payload"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_malformed_and_non_http_locations_fail_without_extra_send() {
        let client = streaming_client().unwrap();
        for location in [
            "file:///tmp/secret",
            "http://[",
            "http://user:password@localhost/final",
        ] {
            let (url, server) = redirect_server(307, location, 1);
            let budget = RetryBudget::new(5, Duration::from_secs(1), None);
            let error = execute_streaming_with_retry_budget(
                &client,
                |c| c.get(&url),
                &CancelToken::new(),
                &budget,
            )
            .await
            .unwrap_err();
            assert!(error.contains("redirect"), "{error}");
            assert!(!error.contains(location));
            assert_eq!(budget.attempts(), 1);
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_destination_cooldown_preserves_cancellation_and_deadline() {
        let client = streaming_client().unwrap();
        for cancel_wait in [false, true] {
            let destination = format!(
                "http://127.0.0.1:9/final-{}",
                if cancel_wait { "cancel" } else { "deadline" }
            );
            let destination_url = reqwest::Url::parse(&destination).unwrap();
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("retry-after", "1".parse().unwrap());
            extend_cooldown(
                quota_key(&destination_url, &reqwest::header::HeaderMap::new()),
                &headers,
            );
            let (url, server) = redirect_server(307, &destination, 1);
            let cancel = CancelToken::new();
            let budget = RetryBudget::new(
                5,
                Duration::from_secs(2),
                if cancel_wait {
                    None
                } else {
                    Some(Instant::now() + Duration::from_millis(500))
                },
            );
            let cancel_clone = cancel.clone();
            let canceller = tokio::spawn(async move {
                if cancel_wait {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    cancel_clone.cancel();
                }
            });
            let error =
                execute_streaming_with_retry_budget(&client, |c| c.get(&url), &cancel, &budget)
                    .await
                    .unwrap_err();
            if cancel_wait {
                assert_eq!(error, "request cancelled");
            } else {
                assert!(error.contains("deadline"), "{error}");
            }
            assert_eq!(budget.attempts(), 1);
            assert_eq!(server.join().unwrap().len(), 1);
            canceller.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redirect_review_request_timeout_bounds_destination_cooldown_wait() {
        let destination = reqwest::Url::parse("http://127.0.0.1:9/request-timeout-final").unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "1".parse().unwrap());
        extend_cooldown(
            quota_key(&destination, &reqwest::header::HeaderMap::new()),
            &headers,
        );
        let (url, server) = redirect_server(307, destination.as_str(), 1);
        let client = streaming_client().unwrap();
        let budget = RetryBudget::new(5, Duration::from_secs(2), None);
        let started = Instant::now();
        let error = execute_streaming_with_retry_budget(
            &client,
            |c| c.get(&url).timeout(Duration::from_millis(200)),
            &CancelToken::new(),
            &budget,
        )
        .await
        .unwrap_err();
        assert!(error.contains("deadline"), "{error}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "cooldown outlived request timeout: {:?}",
            started.elapsed()
        );
        assert_eq!(budget.attempts(), 1);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn gateway_5xx_codes_outside_the_old_allow_list_are_retryable() {
        // The retired allow-list was [429, 500, 502, 503, 504]; Cloudflare's 520-526 and
        // other gateway codes aborted the session instead of retrying (issue #68).
        for status in [
            429, 500, 501, 502, 503, 504, 507, 520, 521, 522, 523, 524, 525, 526, 598,
        ] {
            assert!(is_retryable_status(status), "{status} must be retryable");
        }
        for status in [400, 401, 403, 404, 409, 413, 422, 499] {
            assert!(!is_retryable_status(status), "{status} must not be retried");
        }
    }

    #[test]
    fn backoff_increases_exponentially() {
        let d0 = compute_backoff(0);
        let d1 = compute_backoff(1);
        let d2 = compute_backoff(2);

        assert!(d0.as_millis() >= 900 && d0.as_millis() <= 1100);
        assert!(d1.as_millis() >= 1800 && d1.as_millis() <= 2200);
        assert!(d2.as_millis() >= 3600 && d2.as_millis() <= 4400);
    }

    #[test]
    fn backoff_caps_at_max() {
        let d10 = compute_backoff(10);
        assert!(d10.as_millis() <= 66_000);
    }

    #[test]
    fn jitter_value_is_in_range() {
        let v = jitter_value();
        assert!((0.0..=1.0).contains(&v));
    }

    #[test]
    fn streaming_client_uses_idle_read_timeout() {
        assert_eq!(
            streaming_idle_read_timeout(),
            Duration::from_secs(300),
            "streaming responses should allow long active generations but fail idle reads"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_retry_backoff_stops_when_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry endpoint");
        listener
            .set_nonblocking(true)
            .expect("set retry endpoint nonblocking");
        let address = listener.local_addr().expect("retry endpoint address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let (response_tx, response_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        server_requests.fetch_add(1, Ordering::SeqCst);
                        let mut request = [0u8; 4 * 1024];
                        let _ = stream.read(&mut request);
                        write!(
                            stream,
                            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .expect("write retry response");
                        response_tx.send(()).expect("announce retry response");
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "retry request was not received");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept retry request: {error}"),
                }
            }
        });
        let cancel = CancelToken::new();
        let cancel_after_response = cancel.clone();
        let canceller = thread::spawn(move || {
            response_rx.recv().expect("wait for retry response");
            cancel_after_response.cancel();
        });
        let url = format!("http://{address}/retry");

        // Build the client outside the timed window: constructing the reqwest
        // client (connection pool + hickory DNS resolver) is unrelated to the
        // cancellation latency under test and can be slow on a cold CI runner.
        let client = streaming_client().expect("streaming client");
        let started = Instant::now();
        let result =
            execute_streaming_with_retry(&client, |client| client.get(&url), &cancel).await;
        let elapsed = started.elapsed();

        canceller.join().expect("retry canceller");
        server.join().expect("retry server");
        assert_eq!(result.unwrap_err(), "request cancelled");
        assert!(
            elapsed < Duration::from_millis(250),
            "cancelled retry slept for {elapsed:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_streaming_request_closes_in_flight_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled endpoint");
        let address = listener.local_addr().expect("stalled endpoint address");
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled request");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set request read timeout");
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).expect("read request headers");
                assert!(read > 0, "client closed before request headers completed");
                request.extend_from_slice(&chunk[..read]);
            }
            accepted_tx.send(()).expect("announce accepted request");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read client close: {error}"),
            };
            closed_tx.send(closed).expect("report client close");
        });
        let cancel = CancelToken::new();
        let cancel_after_accept = cancel.clone();
        let canceller = thread::spawn(move || {
            accepted_rx.recv().expect("wait for accepted request");
            cancel_after_accept.cancel();
        });
        let url = format!("http://{address}/stall");

        // Build the client outside the timed window: constructing the reqwest
        // client (connection pool + hickory DNS resolver) is unrelated to the
        // cancellation latency under test and can be slow on a cold CI runner.
        let client = streaming_client().expect("streaming client");
        let started = Instant::now();
        let result =
            execute_streaming_with_retry(&client, |client| client.get(&url), &cancel).await;
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for peer close result");

        canceller.join().expect("stalled request canceller");
        server.join().expect("stalled request server");
        assert_eq!(result.unwrap_err(), "request cancelled");
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled request returned after {elapsed:?}"
        );
        assert!(
            connection_closed,
            "cancelled request left the TCP connection owned by a detached worker"
        );
    }

    /// Serve exactly `retryable_count` responses with `status_line` (each
    /// carrying `Retry-After: 0` so the blocking backoff stays near-instant),
    /// then, if `then_succeed` is true, serve one final `200 OK`. The listener
    /// is non-blocking with a bounded deadline so the server thread can never
    /// block `join()` waiting for a connection the client will not open (e.g.
    /// once the client has already succeeded or exhausted its retries).
    fn spawn_flaky_blocking_server(
        retryable_count: usize,
        status_line: &'static str,
        then_succeed: bool,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind flaky endpoint");
        listener
            .set_nonblocking(true)
            .expect("set flaky endpoint nonblocking");
        let address = listener.local_addr().expect("flaky endpoint address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let expected = retryable_count + usize::from(then_succeed);
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut served = 0;
            while served < expected {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let index = server_requests.fetch_add(1, Ordering::SeqCst);
                        // The accepted socket inherits the listener's non-blocking
                        // mode, so a bare `read()` here returns immediately and we
                        // would reply, then close, before the client's request has
                        // actually arrived. Closing a socket that still has unread
                        // request bytes in its receive buffer emits a TCP RST rather
                        // than a graceful FIN; on Windows reqwest surfaces that RST
                        // as a transport error instead of the retryable status under
                        // test, so the exhaustion path reports "request failed..."
                        // instead of "max retries exceeded". Flip the stream to
                        // blocking and wait (bounded by the server deadline) for the
                        // request headers: this both orders the reply after the request
                        // and drains the request so the close is graceful.
                        let _ = stream.set_nonblocking(false);
                        let read_timeout = deadline
                            .saturating_duration_since(Instant::now())
                            .max(Duration::from_millis(50));
                        let _ = stream.set_read_timeout(Some(read_timeout));
                        let mut request = Vec::new();
                        let mut chunk = [0_u8; 1024];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => request.extend_from_slice(&chunk[..read]),
                            }
                        }
                        let response = if index < retryable_count {
                            format!(
                                "HTTP/1.1 {status_line}\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                                .to_string()
                        };
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        served += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "flaky server only saw {served}/{expected} requests before timeout"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept flaky request: {error}"),
                }
            }
        });
        (format!("http://{address}"), requests, server)
    }

    #[test]
    fn blocking_retry_recovers_after_retryable_statuses() {
        // Two 503s (with Retry-After: 0) then a 200: the retry loop must retry
        // through both and return the eventual success rather than the 503.
        let (base, requests, server) =
            spawn_flaky_blocking_server(2, "503 Service Unavailable", true);
        let url = format!("{base}/retry");

        let result = execute_with_retry(|client| client.get(&url));

        server.join().expect("flaky server");
        assert!(result.is_ok(), "expected eventual success, got {result:?}");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "must retry both 503s before the successful attempt"
        );
    }

    #[test]
    fn blocking_retry_reports_exhaustion_after_max_retries() {
        // Every attempt returns a retryable status, so the loop must give up
        // after MAX_RETRIES and surface the last status instead of looping
        // forever. Retry-After: 0 keeps the exhaustion path near-instant.
        let attempts = (MAX_RETRIES as usize) + 1;
        let (base, requests, server) =
            spawn_flaky_blocking_server(attempts, "500 Internal Server Error", false);
        let url = format!("{base}/exhaust");

        let error = execute_with_retry(|client| client.get(&url)).expect_err("should exhaust");

        server.join().expect("flaky server");
        assert_eq!(
            error,
            "max retries exceeded (last status: 500 Internal Server Error)"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            attempts,
            "must stop after the initial attempt plus MAX_RETRIES"
        );
    }

    #[test]
    fn retry_after_header_drives_the_blocking_backoff_delay() {
        // A 429 carrying Retry-After: 0 must be honored (parsed and used as the
        // sleep) so the loop retries immediately and then succeeds.
        let (base, requests, server) =
            spawn_flaky_blocking_server(1, "429 Too Many Requests", true);
        let url = format!("{base}/throttle");

        let started = Instant::now();
        let result = execute_with_retry(|client| client.get(&url));
        let elapsed = started.elapsed();

        server.join().expect("flaky server");
        assert!(
            result.is_ok(),
            "expected success after honoring Retry-After"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2, "one retry then success");
        // Retry-After: 0 must short-circuit the exponential backoff (which would
        // otherwise sleep ~1s on the first retry).
        assert!(
            elapsed < Duration::from_millis(900),
            "Retry-After: 0 should bypass the ~1s exponential backoff, took {elapsed:?}"
        );
    }

    fn cooldown_server(
        first_retry_after: &str,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<Vec<Instant>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let retry_after = first_retry_after.to_string();
        let server = thread::spawn(move || {
            let mut arrivals = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut headers = Vec::new();
                        let mut chunk = [0; 1024];
                        while !headers.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = socket.read(&mut chunk).unwrap();
                            if n == 0 {
                                break;
                            }
                            headers.extend_from_slice(&chunk[..n]);
                        }
                        arrivals.push(Instant::now());
                        let index = count.fetch_add(1, Ordering::SeqCst);
                        let response = if index == 0 {
                            format!(
                                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {retry_after}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_string()
                        };
                        let _ = socket.write_all(response.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
            arrivals
        });
        (url, requests, server)
    }

    fn assert_pr160_http_date_delay(format: &str) {
        let date = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .format(format)
            .to_string();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", date.parse().unwrap());
        let delay = retry_after_header(&headers).expect("valid HTTP-date must be honored");
        assert!(
            (Duration::from_secs(115)..=Duration::from_secs(120)).contains(&delay),
            "Retry-After {date:?} produced {delay:?} instead of the requested delay"
        );
    }

    #[test]
    fn pr160_retry_after_imf_fixdate() {
        assert_pr160_http_date_delay("%a, %d %b %Y %H:%M:%S GMT");
    }

    #[test]
    fn pr160_retry_after_rfc850() {
        assert_pr160_http_date_delay("%A, %d-%b-%y %H:%M:%S GMT");
    }

    #[test]
    fn pr160_retry_after_asctime() {
        assert_pr160_http_date_delay("%a %b %e %H:%M:%S %Y");
    }

    #[test]
    fn pr160_retry_after_rfc850_uses_http_fifty_year_window() {
        use chrono::{Datelike, TimeZone};
        let now = chrono::Utc::now();
        // RFC 850 has a sliding HTTP year window, unlike a fixed 1970 pivot.
        let future = chrono::Utc
            .with_ymd_and_hms(now.year() + 45, 1, 1, 0, 0, 0)
            .unwrap();
        let value = future.format("%A, %d-%b-%y %H:%M:%S GMT").to_string();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", value.parse().unwrap());
        let delay = retry_after_header(&headers).expect("valid RFC 850 date");
        let expected = (future - now).to_std().unwrap();
        assert!(delay <= expected && expected - delay < Duration::from_secs(5));

        let past = chrono::Utc
            .with_ymd_and_hms(now.year() - 40, 1, 1, 0, 0, 0)
            .unwrap();
        headers.insert(
            "retry-after",
            past.format("%A, %d-%b-%y %H:%M:%S GMT")
                .to_string()
                .parse()
                .unwrap(),
        );
        assert_eq!(retry_after_header(&headers), Some(Duration::ZERO));
    }

    #[test]
    fn shared_retry_http_date_is_parsed_without_a_new_dependency() {
        let mut headers = reqwest::header::HeaderMap::new();
        let date = (chrono::Utc::now() + chrono::Duration::seconds(3))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        headers.insert("retry-after", date.parse().unwrap());
        let delay = retry_after_header(&headers).expect("HTTP-date Retry-After must be honored");
        assert!(delay >= Duration::from_secs(1) && delay <= Duration::from_secs(3));
        headers.insert("retry-after", "0".parse().unwrap());
        assert_eq!(retry_after_header(&headers), Some(Duration::ZERO));
        headers.insert(
            "retry-after",
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after_header(&headers), Some(Duration::ZERO));
        headers.insert("retry-after", "invalid".parse().unwrap());
        assert_eq!(retry_after_header(&headers), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_retry_last_summary_response_cools_children_blocking_and_summary() {
        let (url, _, server) = cooldown_server("1");
        let client = streaming_client().unwrap();
        let cancel = CancelToken::new();
        execute_streaming_once(client.get(&url).bearer_auth("quota-a"), &cancel)
            .await
            .expect_err("first summary throttled");
        let other = execute_streaming_once(client.get(&url).bearer_auth("quota-b"), &cancel);
        let child =
            execute_streaming_with_retry(&client, |c| c.get(&url).bearer_auth("quota-a"), &cancel);
        let summary = execute_streaming_once(client.get(&url).bearer_auth("quota-a"), &cancel);
        let blocking_url = url.clone();
        let blocking = tokio::task::spawn_blocking(move || {
            execute_with_retry(|c| c.get(&blocking_url).bearer_auth("quota-a"))
        });
        let (other, child, summary) = tokio::join!(other, child, summary);
        assert!(other.is_ok() && child.is_ok() && summary.is_ok());
        assert!(blocking.await.unwrap().is_ok());
        let arrivals = server.join().unwrap();
        assert_eq!(arrivals.len(), 5);
        assert!(
            arrivals[1].duration_since(arrivals[0]) < Duration::from_millis(500),
            "independent credentials must remain independent"
        );
        for at in &arrivals[2..] {
            assert!(
                at.duration_since(arrivals[0]) >= Duration::from_millis(990),
                "same-quota request sent before Retry-After expired"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_retry_wait_is_cancelled_before_another_send() {
        let (url, requests, server) = cooldown_server("1");
        let client = streaming_client().unwrap();
        let cancel = CancelToken::new();
        execute_streaming_once(client.get(&url).bearer_auth("quota-a"), &cancel)
            .await
            .expect_err("first summary throttled");
        let cancel_later = cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_later.cancel();
        });
        let started = Instant::now();
        let result = execute_streaming_once(client.get(&url).bearer_auth("quota-a"), &cancel).await;
        canceller.await.unwrap();
        assert_eq!(result.unwrap_err(), "request cancelled");
        assert!(started.elapsed() < Duration::from_millis(250));
        server.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_retry_cooldown_spaces_waiting_sends() {
        let (url, _, server) = cooldown_server("1");
        let client = streaming_client().unwrap();
        let cancel = CancelToken::new();
        execute_streaming_once(client.get(&url).bearer_auth("space-quota"), &cancel)
            .await
            .unwrap_err();
        let calls = (0..3)
            .map(|_| {
                let client = client.clone();
                let cancel = cancel.clone();
                let url = url.clone();
                tokio::spawn(async move {
                    execute_streaming_once(client.get(url).bearer_auth("space-quota"), &cancel)
                        .await
                })
            })
            .collect::<Vec<_>>();
        for call in calls {
            call.await.unwrap().unwrap();
        }
        let arrivals = server.join().unwrap();
        assert_eq!(arrivals.len(), 4);
        for pair in arrivals[1..].windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= Duration::from_millis(15),
                "waiters retried simultaneously at cooldown expiry"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_retry_deadline_and_wait_budget_never_allow_early_sends() {
        let (url, count, server) = cooldown_server("1");
        let client = streaming_client().unwrap();
        let cancel = CancelToken::new();
        execute_streaming_once(client.get(&url).bearer_auth("quota-deadline"), &cancel)
            .await
            .unwrap_err();
        let started = Instant::now();
        let deadline = RetryBudget::new(
            5,
            Duration::from_secs(2),
            Some(started + Duration::from_millis(80)),
        );
        let error = execute_streaming_with_retry_budget(
            &client,
            |c| c.get(&url).bearer_auth("quota-deadline"),
            &cancel,
            &deadline,
        )
        .await
        .unwrap_err();
        assert!(error.contains("deadline"), "{error}");
        let wait = RetryBudget::new(5, Duration::from_millis(80), None);
        let error = execute_streaming_with_retry_budget(
            &client,
            |c| c.get(&url).bearer_auth("quota-deadline"),
            &cancel,
            &wait,
        )
        .await
        .unwrap_err();
        assert!(error.contains("wait budget"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(deadline.attempts(), 0);
        assert_eq!(wait.attempts(), 0);
        server.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_retry_unrepresentable_retry_after_cannot_fall_back_to_early_retry() {
        let (url, count, server) = cooldown_server("18446744073709551615");
        let client = streaming_client().unwrap();
        let cancel = CancelToken::new();
        execute_streaming_once(client.get(&url).bearer_auth("quota-huge"), &cancel)
            .await
            .unwrap_err();
        let budget = RetryBudget::new(
            5,
            Duration::from_secs(5),
            Some(Instant::now() + Duration::from_millis(80)),
        );
        let result = execute_streaming_with_retry_budget(
            &client,
            |c| c.get(&url).bearer_auth("quota-huge"),
            &cancel,
            &budget,
        )
        .await;
        assert!(
            result.is_err(),
            "overflowing server wait must never permit an early send"
        );
        server.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
