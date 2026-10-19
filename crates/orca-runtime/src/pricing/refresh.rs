//! Refreshes DeepSeek's prices from models.dev, which records the off-peak
//! price: at most once a day, in the background, never blocking a turn.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use flate2::read::GzDecoder;
use orca_core::model::canonical_model_name;
use serde::Deserialize;
use serde_json::Value;

use super::book::{self, CACHE_VERSION, CachedRates, PricingCache};
use super::rates::Rates;

pub const DEFAULT_URL: &str = "https://models.dev/api.json";
/// Replaces [`DEFAULT_URL`]; also lets a development build refresh, which is
/// how tests point it at a local fixture.
pub const URL_ENV: &str = "ORCA_PRICING_URL";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum RefreshOutcome {
    Updated(PricingCache),
    NotModified(PricingCache),
    Failed(String),
}

#[derive(Deserialize)]
struct ModelsDevApi {
    deepseek: Option<ModelsDevProvider>,
}

#[derive(Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: BTreeMap<String, Value>,
}

/// DeepSeek's off-peak prices in a models.dev `api.json` body, by canonical
/// model name. Incomplete and implausible entries are skipped; a canonical
/// entry wins over an alias of it.
pub fn parse_models_dev(
    body: &[u8],
    fetched_at: DateTime<Utc>,
) -> Result<BTreeMap<String, CachedRates>, String> {
    let api: ModelsDevApi = serde_json::from_slice(body)
        .map_err(|error| format!("invalid models.dev JSON: {error}"))?;
    let provider = api
        .deepseek
        .ok_or_else(|| "models.dev lists no deepseek provider".to_string())?;
    let mut prices = BTreeMap::new();
    let fetch_date = fetched_at.date_naive();
    for (id, model) in provider.models {
        let Some(cost) = model.get("cost") else {
            continue;
        };
        let (Some(input), Some(cache_read), Some(output)) = (
            cost.get("input").and_then(Value::as_f64),
            cost.get("cache_read").and_then(Value::as_f64),
            cost.get("output").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let rates = Rates {
            input_cache_miss: input,
            input_cache_hit: cache_read,
            output,
        };
        let canonical = canonical_model_name(&id);
        if !book::rates_are_plausible(canonical, rates) {
            continue;
        }
        if id == canonical || !prices.contains_key(canonical) {
            let updated = model
                .get("last_updated")
                .and_then(Value::as_str)
                .filter(|text| text.len() == 10 && text.is_ascii())
                .and_then(|text| {
                    NaiveDate::parse_from_str(text, "%Y-%m-%d")
                        .ok()
                        .filter(|date| date.to_string() == text)
                })
                .filter(|date| *date <= fetch_date)
                .unwrap_or(fetch_date);
            prices.insert(canonical.to_string(), CachedRates { rates, updated });
        }
    }
    if prices.is_empty() {
        return Err("models.dev lists no plausible deepseek prices".to_string());
    }
    Ok(prices)
}

pub fn fetch(url: &str, previous: Option<&PricingCache>, now: DateTime<Utc>) -> RefreshOutcome {
    fetch_with_limits(url, previous, now, REQUEST_TIMEOUT, MAX_BODY_BYTES)
}

/// One conditional GET of `url`, sending `previous`'s ETag.
pub fn fetch_with_limits(
    url: &str,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
    timeout: Duration,
    max_body_bytes: u64,
) -> RefreshOutcome {
    match try_fetch(url, previous, now, timeout, max_body_bytes) {
        Ok(outcome) => outcome,
        Err(error) => RefreshOutcome::Failed(error),
    }
}

fn try_fetch(
    url: &str,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
    timeout: Duration,
    max_body_bytes: u64,
) -> Result<RefreshOutcome, String> {
    use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, ETAG, IF_NONE_MATCH, USER_AGENT};

    let client = orca_mcp::http::blocking_client()?;
    let mut request = client
        .get(url)
        .timeout(timeout)
        .header(ACCEPT_ENCODING, "gzip")
        .header(USER_AGENT, "orca-pricing");
    if let Some(etag) = previous.and_then(|cache| cache.etag.as_deref()) {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .map_err(|error| format!("pricing request failed: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        let mut cache = previous
            .cloned()
            .ok_or_else(|| "pricing source answered 304 with no cached copy".to_string())?;
        cache.fetched_at = now;
        return Ok(RefreshOutcome::NotModified(cache));
    }
    if !status.is_success() {
        return Err(format!("pricing request returned HTTP {status}"));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let gzip = response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("gzip"));
    let raw = read_limited(response, max_body_bytes)?;
    let body = if gzip {
        read_limited(GzDecoder::new(raw.as_slice()), max_body_bytes)?
    } else {
        raw
    };
    let models = parse_models_dev(&body, now)?;
    Ok(RefreshOutcome::Updated(PricingCache {
        version: CACHE_VERSION,
        source: url.to_string(),
        fetched_at: now,
        etag,
        models,
    }))
}

fn read_limited(reader: impl Read, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading the pricing response failed: {error}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "pricing response is too large (over {max_bytes} bytes)"
        ));
    }
    Ok(bytes)
}

/// How long a cached copy counts as fresh.
fn refresh_after() -> TimeDelta {
    TimeDelta::hours(24)
}

/// The URL to refresh from now, if any: refresh is on, the build is not a
/// development one (unless a URL is named), and the cache is missing or a day old.
pub fn refresh_url(
    enabled: bool,
    url_override: Option<String>,
    dev_build: bool,
    cached: Option<&PricingCache>,
    now: DateTime<Utc>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let url_override = url_override.filter(|url| !url.trim().is_empty());
    if url_override.is_none() && dev_build {
        return None;
    }
    if cached.is_some_and(|cache| now.signed_duration_since(cache.fetched_at) < refresh_after()) {
        return None;
    }
    Some(url_override.unwrap_or_else(|| DEFAULT_URL.to_string()))
}

/// Fetches and caches; a cache that cannot be written still counts, so the
/// caller can use the prices for this process.
pub fn refresh_and_store(
    url: &str,
    path: &Path,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
) -> RefreshOutcome {
    let outcome = fetch(url, previous, now);
    if let RefreshOutcome::Updated(cache) | RefreshOutcome::NotModified(cache) = &outcome {
        let _ = book::write_cache(path, cache);
    }
    outcome
}

/// Starts at most one background refresh per process, when it is due.
pub(crate) fn ensure_background_refresh(enabled: bool) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !enabled || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(path) = book::cache_path() else {
        return;
    };
    let cached = book::read_cache(&path);
    let Some(url) = refresh_url(
        true,
        std::env::var(URL_ENV).ok(),
        crate::update_check::is_dev_build_run(),
        cached.as_ref(),
        Utc::now(),
    ) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("orca-pricing-refresh".to_string())
        .spawn(move || {
            if let RefreshOutcome::Updated(cache) | RefreshOutcome::NotModified(cache) =
                refresh_and_store(&url, &path, cached.as_ref(), Utc::now())
            {
                book::install(&cache);
            }
        });
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;
    use crate::pricing::book::read_cache;
    use crate::pricing::rates::{FLASH_RATES, PRO_RATES};
    use orca_core::model::{FLASH_MODEL, PRO_MODEL};

    /// The deepseek provider from models.dev's api.json on 2026-10-19, with one
    /// unrelated provider to skip.
    const MODELS_DEV: &str = r#"{
      "openai": {"id": "openai", "models": {"gpt-x": {"cost": {"input": 1.0, "output": 2.0}}}},
      "deepseek": {"id": "deepseek", "api": "https://api.deepseek.com", "models": {
        "deepseek-flash": {"id": "deepseek-flash", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}},
        "deepseek-v4-flash": {"id": "deepseek-v4-flash", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}},
        "deepseek-v4-pro": {"id": "deepseek-v4-pro", "cost": {"input": 0.66, "output": 1.98, "reasoning": 1.98, "cache_read": 0.022}},
        "deepseek-v4-flash-vision-exp": {"id": "deepseek-v4-flash-vision-exp", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}}
      }}
    }"#;

    fn now() -> DateTime<Utc> {
        "2026-10-20T08:00:00Z".parse().unwrap()
    }

    enum Reply {
        Send(Vec<u8>),
        Stall,
    }

    /// A blocking local HTTP server answering successive connections with
    /// `replies`, one each; joins to the request heads it read (lowercased).
    fn serve(replies: Vec<Reply>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind pricing fixture");
        let url = format!("http://{}/api.json", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().expect("accept pricing request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                heads.push(head.to_ascii_lowercase());
                match reply {
                    Reply::Send(bytes) => {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                    Reply::Stall => std::thread::sleep(Duration::from_secs(10)),
                }
            }
            heads
        });
        (url, server)
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        Reply::Send(bytes)
    }

    fn gzip(body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn cached(fetched_at: &str, etag: Option<&str>) -> PricingCache {
        PricingCache {
            version: CACHE_VERSION,
            source: DEFAULT_URL.to_string(),
            fetched_at: fetched_at.parse().unwrap(),
            etag: etag.map(str::to_string),
            models: BTreeMap::from([(
                FLASH_MODEL.to_string(),
                CachedRates {
                    rates: FLASH_RATES,
                    updated: fetched_at.parse::<DateTime<Utc>>().unwrap().date_naive(),
                },
            )]),
        }
    }

    #[test]
    fn fetch_records_content_dates_and_lenient_fallbacks() {
        for (last_updated, expected) in [
            (Some(serde_json::json!("2026-10-18")), "2026-10-18"),
            (None, "2026-10-20"),
            (Some(serde_json::json!("not a date")), "2026-10-20"),
            (Some(serde_json::json!("2026-02-30")), "2026-10-20"),
            (Some(serde_json::json!("2026-1-2")), "2026-10-20"),
            (Some(serde_json::json!("-0001-01-01")), "2026-10-20"),
            (Some(serde_json::json!(123)), "2026-10-20"),
            (Some(serde_json::json!("2026-10-21")), "2026-10-20"),
        ] {
            let mut body: Value = serde_json::from_str(MODELS_DEV).unwrap();
            if let Some(date) = last_updated {
                body["deepseek"]["models"][FLASH_MODEL]["last_updated"] = date;
            }
            let (url, server) = serve(vec![response(
                "200 OK",
                &[],
                &serde_json::to_vec(&body).unwrap(),
            )]);
            let outcome = fetch(&url, None, now());
            server.join().unwrap();
            let RefreshOutcome::Updated(cache) = outcome else {
                panic!("expected updated prices, got {outcome:?}");
            };
            let value = serde_json::to_value(cache).unwrap();
            assert_eq!(value["models"][FLASH_MODEL]["updated"], expected);
        }
    }

    #[test]
    fn parse_keeps_canonical_deepseek_prices() {
        let prices = parse_models_dev(MODELS_DEV.as_bytes(), now()).unwrap();
        assert_eq!(
            prices.get(FLASH_MODEL).map(|entry| entry.rates),
            Some(FLASH_RATES)
        );
        assert_eq!(
            prices.get(PRO_MODEL).map(|entry| entry.rates),
            Some(PRO_RATES)
        );
        assert_eq!(
            prices.len(),
            2,
            "aliases fold into canonical names: {prices:?}"
        );
    }

    #[test]
    fn parse_prefers_the_canonical_entry_over_an_alias() {
        let body = r#"{"deepseek": {"models": {
          "deepseek-v4-flash": {"cost": {"input": 0.2, "output": 0.8, "cache_read": 0.004}},
          "deepseek-flash": {"cost": {"input": 0.15, "output": 0.6, "cache_read": 0.003}}
        }}}"#;
        let prices = parse_models_dev(body.as_bytes(), now()).unwrap();
        assert_eq!(
            prices.get(FLASH_MODEL).map(|entry| entry.rates),
            Some(FLASH_RATES)
        );
    }

    #[test]
    fn parse_skips_malformed_entries_without_losing_well_formed_models() {
        let body = r#"{"deepseek": {"models": {
          "deepseek-flash": {"cost": {"input": 0.15, "cache_read": 0.003, "output": 0.6}},
          "bad-string-input": {"cost": {"input": "0.15", "cache_read": 0.003, "output": 0.6}},
          "bad-null-cost": {"cost": null},
          "bad-object-input": {"cost": {"input": {"tier": 1}, "cache_read": 0.003, "output": 0.6}},
          "bad-non-object": "not an object"
        }}}"#;
        let prices = parse_models_dev(body.as_bytes(), now()).unwrap();
        assert_eq!(
            prices,
            BTreeMap::from([(
                FLASH_MODEL.to_string(),
                CachedRates {
                    rates: FLASH_RATES,
                    updated: now().date_naive()
                }
            )])
        );

        let mut malformed_only: serde_json::Value = serde_json::from_str(body).unwrap();
        malformed_only["deepseek"]["models"]
            .as_object_mut()
            .unwrap()
            .remove(FLASH_MODEL);
        assert!(parse_models_dev(&serde_json::to_vec(&malformed_only).unwrap(), now()).is_err());
    }

    #[test]
    fn parse_skips_incomplete_or_implausible_entries_and_rejects_empty_results() {
        let body = r#"{"deepseek": {"models": {
          "deepseek-flash": {"cost": {"input": 0.15e-6, "output": 0.6e-6, "cache_read": 0.003e-6}},
          "deepseek-v4-pro": {"cost": {"input": 0.66, "output": 1.98}},
          "deepseek-v5-flash": {"cost": {"input": 0.3, "output": 1.2, "cache_read": 0.006}}
        }}}"#;
        let prices = parse_models_dev(body.as_bytes(), now()).unwrap();
        assert_eq!(prices.keys().collect::<Vec<_>>(), ["deepseek-v5-flash"]);

        assert!(parse_models_dev(br#"{"openai": {"models": {}}}"#, now()).is_err());
        assert!(parse_models_dev(br#"{"deepseek": {"models": {}}}"#, now()).is_err());
        assert!(parse_models_dev(b"not json", now()).is_err());
    }

    #[test]
    fn fetch_reads_gzip_json_and_records_the_etag() {
        let (url, server) = serve(vec![response(
            "200 OK",
            &[("Content-Encoding", "gzip"), ("ETag", "W/\"v1\"")],
            &gzip(MODELS_DEV.as_bytes()),
        )]);
        let outcome = fetch(&url, None, now());
        let RefreshOutcome::Updated(cache) = outcome else {
            panic!("expected an update, got {outcome:?}");
        };
        assert_eq!(cache.version, CACHE_VERSION);
        assert_eq!(cache.source, url);
        assert_eq!(cache.fetched_at, now());
        assert_eq!(cache.etag.as_deref(), Some("W/\"v1\""));
        assert_eq!(
            cache.models.get(FLASH_MODEL).map(|entry| entry.rates),
            Some(FLASH_RATES)
        );
        let heads = server.join().unwrap();
        assert!(heads[0].contains("accept-encoding: gzip"), "{heads:?}");
        assert!(heads[0].contains("user-agent: orca-pricing"), "{heads:?}");
        assert!(!heads[0].contains("if-none-match"), "{heads:?}");
    }

    #[test]
    fn fetch_accepts_an_uncompressed_body() {
        let (url, server) = serve(vec![response("200 OK", &[], MODELS_DEV.as_bytes())]);
        assert!(matches!(
            fetch(&url, None, now()),
            RefreshOutcome::Updated(_)
        ));
        server.join().unwrap();
    }

    #[test]
    fn fetch_sends_the_etag_and_keeps_the_cache_on_304() {
        let (url, server) = serve(vec![response("304 Not Modified", &[], b"")]);
        let previous = cached("2026-10-18T08:00:00Z", Some("\"v1\""));
        let outcome = fetch(&url, Some(&previous), now());
        let RefreshOutcome::NotModified(cache) = outcome else {
            panic!("expected not-modified, got {outcome:?}");
        };
        assert_eq!(cache.models, previous.models);
        assert_eq!(cache.etag, previous.etag);
        assert_eq!(cache.fetched_at, now());
        let heads = server.join().unwrap();
        assert!(heads[0].contains("if-none-match: \"v1\""), "{heads:?}");
    }

    #[test]
    fn fetch_fails_cleanly_on_bad_responses() {
        let (url, server) = serve(vec![
            response("500 Internal Server Error", &[], b"oops"),
            response("200 OK", &[("Content-Encoding", "gzip")], b"not gzip"),
            response("304 Not Modified", &[], b""),
            response("200 OK", &[], &[b' '; 4096]),
        ]);
        assert!(matches!(
            fetch(&url, None, now()),
            RefreshOutcome::Failed(_)
        ));
        assert!(matches!(
            fetch(&url, None, now()),
            RefreshOutcome::Failed(_)
        ));
        // 304 without a cached copy to keep.
        assert!(matches!(
            fetch(&url, None, now()),
            RefreshOutcome::Failed(_)
        ));
        let too_large = fetch_with_limits(&url, None, now(), Duration::from_secs(5), 1024);
        assert!(
            matches!(too_large, RefreshOutcome::Failed(ref error) if error.contains("too large")),
            "{too_large:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn fetch_gives_up_at_its_timeout() {
        let (url, _server) = serve(vec![Reply::Stall]);
        let started = Instant::now();
        let outcome = fetch_with_limits(&url, None, now(), Duration::from_millis(500), 1024);
        assert!(matches!(outcome, RefreshOutcome::Failed(_)), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn refresh_url_follows_the_switch_the_build_and_the_cache_age() {
        let fresh = cached("2026-10-20T02:00:00Z", None);
        let stale = cached("2026-10-19T07:00:00Z", None);
        let custom = || Some("http://127.0.0.1:9/api.json".to_string());
        assert_eq!(refresh_url(false, None, false, None, now()), None);
        assert_eq!(refresh_url(true, None, true, None, now()), None);
        assert_eq!(refresh_url(true, custom(), true, None, now()), custom());
        assert_eq!(
            refresh_url(true, Some("  ".to_string()), true, None, now()),
            None
        );
        assert_eq!(refresh_url(true, None, false, Some(&fresh), now()), None);
        assert_eq!(
            refresh_url(true, None, false, Some(&stale), now()),
            Some(DEFAULT_URL.to_string())
        );
        assert_eq!(
            refresh_url(true, None, false, None, now()),
            Some(DEFAULT_URL.to_string())
        );
    }

    #[test]
    fn refresh_and_store_writes_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing-cache.json");
        let (url, server) = serve(vec![
            response("200 OK", &[("ETag", "\"v2\"")], MODELS_DEV.as_bytes()),
            response("304 Not Modified", &[], b""),
        ]);
        let first = refresh_and_store(&url, &path, None, now());
        assert!(matches!(first, RefreshOutcome::Updated(_)));
        let stored = read_cache(&path).expect("stored cache");
        assert_eq!(stored.etag.as_deref(), Some("\"v2\""));

        let later: DateTime<Utc> = "2026-10-21T09:00:00Z".parse().unwrap();
        let second = refresh_and_store(&url, &path, Some(&stored), later);
        assert!(matches!(second, RefreshOutcome::NotModified(_)));
        assert_eq!(read_cache(&path).unwrap().fetched_at, later);
        server.join().unwrap();
    }

    #[test]
    fn refresh_and_store_succeeds_when_the_cache_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("pricing-cache.json");
        let (url, server) = serve(vec![response("200 OK", &[], MODELS_DEV.as_bytes())]);
        assert!(matches!(
            refresh_and_store(&url, &path, None, now()),
            RefreshOutcome::Updated(_)
        ));
        assert!(!path.exists());
        server.join().unwrap();
    }
}
