use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_core::cancel::CancelToken;
use orca_core::provider_types::ProviderError;

const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(8);
const DEFAULT_JITTER_FACTOR: f64 = 0.1;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
        if !error.is_retryable() || completed_attempts >= self.max_attempts {
            return ProviderRetryDecision::Stop;
        }
        ProviderRetryDecision::RetryAfter(self.backoff(completed_attempts - 1))
    }

    fn backoff(&self, retry_index: u32) -> Duration {
        let multiplier = 2_f64.powi(retry_index.min(30) as i32);
        let base = self.initial_backoff.as_secs_f64() * multiplier;
        let capped = base.min(self.max_backoff.as_secs_f64());
        let jitter = 1.0 + (jitter_value() * 2.0 - 1.0) * self.jitter_factor;
        Duration::from_secs_f64((capped * jitter).max(0.0))
    }
}

pub(crate) fn wait_for_provider_retry(delay: Duration, cancel: &CancelToken) -> bool {
    let deadline = std::time::Instant::now() + delay;
    while !cancel.is_cancelled() {
        let now = std::time::Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(CANCELLATION_POLL_INTERVAL));
    }
    false
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
            ProviderErrorKind::StreamClosed,
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
}
