//! Shared HTTP client and retry policy (implementation notes §4, spec §3 "retry").
//!
//! One [`reqwest::Client`] is built at startup and cloned into every stage that
//! talks to the network (Miniflux, social, extraction, images, world briefing).

use std::time::Duration;

/// Descriptive UA required by Reddit and polite everywhere else (§3.4, notes §4).
pub const USER_AGENT: &str = "the-daily-epub/1.0 (personal rss digest; contact tyler@hallada.net)";

/// Default per-request timeout (§3.3: 10s).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the process-wide HTTP client: rustls, gzip, no cookie jar (notes §4).
pub fn build_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .gzip(true)
        // No cookie jar: the `cookies` feature is deliberately off (notes §4).
        .build()
}

/// Jittered exponential backoff, max 3 attempts (crate table "retry").
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt `attempt` (1-based), with ±25% jitter.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exp = self
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
        let capped = exp.min(self.max_delay);
        let jitter = rand::random_range(0.75f64..1.25f64);
        Duration::from_secs_f64(capped.as_secs_f64() * jitter).min(self.max_delay)
    }

    /// Run `op` until it succeeds or returns a non-retryable error.
    ///
    /// `op` is retried while it yields an error for which `retryable` is true —
    /// network failures and 5xx responses (§3.1).
    pub async fn run<T, E, F, Fut>(
        &self,
        what: &str,
        retryable: impl Fn(&E) -> bool,
        mut op: F,
    ) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut attempt = 1;
        loop {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) if attempt < self.max_attempts && retryable(&e) => {
                    let delay = self.delay_for(attempt);
                    tracing::warn!(
                        attempt,
                        max = self.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        "{what} failed, retrying: {e}"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// True for network-level failures and 5xx/429 responses (§3.1, §3.4).
pub fn is_retryable(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() || err.is_request() {
        return true;
    }
    match err.status() {
        Some(s) => s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds() {
        build_client(DEFAULT_TIMEOUT).unwrap();
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = RetryPolicy::default();
        for attempt in 1..=3 {
            let d = p.delay_for(attempt);
            assert!(d <= p.max_delay);
            assert!(d >= Duration::from_millis(300));
        }
        // Second attempt doubles the base delay before jitter (1000ms ± 25%).
        assert!(p.delay_for(2) >= Duration::from_millis(750));
        // Overflow-safe and still capped for absurd attempt numbers.
        let huge = p.delay_for(30);
        assert!(huge <= p.max_delay && huge >= Duration::from_secs(7));
    }

    #[tokio::test]
    async fn retries_until_success_then_stops() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let mut calls = 0;
        let out: Result<u8, String> = policy
            .run(
                "test",
                |_| true,
                || {
                    calls += 1;
                    let n = calls;
                    async move {
                        if n < 3 {
                            Err("boom".to_string())
                        } else {
                            Ok(7u8)
                        }
                    }
                },
            )
            .await;
        assert_eq!(out, Ok(7));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let mut calls = 0;
        let out: Result<u8, String> = policy
            .run(
                "test",
                |_| true,
                || {
                    calls += 1;
                    async { Err("boom".to_string()) }
                },
            )
            .await;
        assert!(out.is_err());
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn non_retryable_errors_fail_fast() {
        let policy = RetryPolicy::default();
        let mut calls = 0;
        let out: Result<u8, String> = policy
            .run(
                "test",
                |_| false,
                || {
                    calls += 1;
                    async { Err("nope".to_string()) }
                },
            )
            .await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }
}
