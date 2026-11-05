use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_core::provider_types::{ProviderError, ProviderErrorKind};
use orca_provider::http_client::RetryBudget;

const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const STREAM_CLOSED_MAX_ATTEMPTS: u32 = 2;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(8);
const DEFAULT_JITTER_FACTOR: f64 = 0.1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRetryDecision {
    RetryAfter(Duration),
    Stop,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderRetryPolicy {
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    jitter_factor: f64,
}

impl Default for ProviderRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            jitter_factor: DEFAULT_JITTER_FACTOR,
        }
    }
}

impl ProviderRetryPolicy {
    pub(crate) fn decide(
        &self,
        error: &ProviderError,
        completed_attempts: u32,
    ) -> ProviderRetryDecision {
        let max_attempts = if error.kind == ProviderErrorKind::StreamClosed {
            STREAM_CLOSED_MAX_ATTEMPTS
        } else {
            self.max_attempts
        };
        if !error.is_retryable() || completed_attempts >= max_attempts {
            return ProviderRetryDecision::Stop;
        }
        ProviderRetryDecision::RetryAfter(self.backoff(completed_attempts - 1))
    }

    pub(crate) fn decide_with_budget(
        &self,
        error: &ProviderError,
        completed_attempts: u32,
        budget: &RetryBudget,
    ) -> ProviderRetryDecision {
        if !budget.can_retry() {
            return ProviderRetryDecision::Stop;
        }
        self.decide(error, completed_attempts)
    }

    fn backoff(&self, retry_index: u32) -> Duration {
        let multiplier = 2_f64.powi(retry_index.min(30) as i32);
        let base = self.initial_backoff.as_secs_f64() * multiplier;
        let capped = base.min(self.max_backoff.as_secs_f64());
        let jitter = 1.0 + (jitter_value() * 2.0 - 1.0) * self.jitter_factor;
        Duration::from_secs_f64((capped * jitter).max(0.0))
    }
}

fn jitter_value() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos as f64) / (u32::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::{ProviderRetryDecision, ProviderRetryPolicy};
    use orca_core::provider_types::{ProviderError, ProviderErrorKind};

    #[test]
    fn transient_errors_retry_until_attempt_budget_is_exhausted_bits_spec_ut() {
        let policy = ProviderRetryPolicy::default();
        for kind in [
            ProviderErrorKind::Transport,
            ProviderErrorKind::Timeout,
            ProviderErrorKind::Server,
            ProviderErrorKind::RateLimit,
            ProviderErrorKind::EmptyResponse,
        ] {
            let error = ProviderError::new(kind, "transient");
            assert!(matches!(
                policy.decide(&error, 1),
                ProviderRetryDecision::RetryAfter(_)
            ));
            assert_eq!(policy.decide(&error, 5), ProviderRetryDecision::Stop);
        }
    }

    #[test]
    fn permanent_or_unsafe_errors_do_not_retry_bits_spec_ut() {
        let policy = ProviderRetryPolicy::default();
        for kind in [
            ProviderErrorKind::MalformedResponse,
            ProviderErrorKind::ContextExceeded,
            ProviderErrorKind::Cancelled,
            ProviderErrorKind::Other,
        ] {
            assert_eq!(
                policy.decide(&ProviderError::new(kind, "terminal"), 1),
                ProviderRetryDecision::Stop
            );
        }
    }

    #[test]
    fn truncated_streams_retry_once_but_do_not_loop_indefinitely() {
        let policy = ProviderRetryPolicy::default();
        let error = ProviderError::new(ProviderErrorKind::StreamClosed, "truncated");

        assert!(matches!(
            policy.decide(&error, 1),
            ProviderRetryDecision::RetryAfter(_)
        ));
        assert_eq!(policy.decide(&error, 2), ProviderRetryDecision::Stop);
    }

    #[test]
    fn shared_retry_nested_runtime_and_http_bound_actual_sends() {
        use orca_core::{
            cancel::CancelToken,
            config::{ProviderKind, ReasoningEffort},
            conversation::Conversation,
        };
        use orca_provider::{ProviderConfig, ProviderStreamEvent};
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sends = Arc::new(AtomicUsize::new(0));
        let count = sends.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !server_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut bytes = Vec::new();
                        let mut chunk = [0; 4096];
                        while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = socket.read(&mut chunk).unwrap();
                            if n == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&chunk[..n]);
                        }
                        let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                        let length = String::from_utf8_lossy(&bytes[..end])
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        while bytes.len() < end + length {
                            let n = socket.read(&mut chunk).unwrap();
                            if n == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&chunk[..n]);
                        }
                        count.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        });
        let config = ProviderConfig {
            api_key: Some("test-quota".into()),
            base_url: Some(base),
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: Some(128),
        };
        let mut conversation = Conversation::new();
        conversation.add_user("hello".into());
        let cancel = CancelToken::new();
        let policy = ProviderRetryPolicy::default();
        let mut completed = 0;
        let budget = orca_provider::http_client::RetryBudget::default();
        loop {
            completed += 1;
            let mut stream = orca_provider::start_streaming_with_retry_budget(
                ProviderKind::DeepSeek,
                &conversation,
                &config,
                cancel.clone(),
                budget.clone(),
            );
            let response = loop {
                match stream.recv_timeout(Duration::from_secs(2)).unwrap() {
                    ProviderStreamEvent::Step(_) => {}
                    ProviderStreamEvent::Completed(response) => break response,
                }
            };
            if policy.decide_with_budget(response.error().unwrap(), completed, &budget)
                == ProviderRetryDecision::Stop
            {
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert_eq!(
            sends.load(Ordering::SeqCst),
            5,
            "one logical operation must never multiply runtime retries by HTTP retries"
        );
    }
}
