//! CDN cache purge (spec §3.12).
//!
//! The origin sends `s-maxage=86400` on the public pages and the feed, so the
//! edge holds a copy for a day. That is only safe because publishing a new
//! issue purges the edge immediately afterwards.
//!
//! The purge is deliberately a *purge everything*, not a list of URLs: a new
//! issue changes more than its own page. `/` becomes the new issue, `/issues`
//! grows a row, `/feed.xml` and `/issues.json` change, and the *previous*
//! issue's page changes too — its "latest" nav marker moves. Enumerating that
//! set correctly is exactly the kind of thing that silently rots. Everything
//! expensive at the edge (`/static/*`) is content-hashed and carries a
//! versioned URL, so re-fetching it after a purge costs one origin hit.

use std::time::Duration;

use crate::config::{CDN_CLOUDFLARE, Config};
use crate::http::{RetryPolicy, build_client};

/// Cloudflare's API root; overridden in tests to point at a loopback server.
pub const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// A purge is a single small request; do not let it hold up a finished run.
const PURGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum CdnError {
    #[error("cdn.provider {0:?} is not recognised")]
    UnknownProvider(String),
    #[error("cdn.cloudflare_zone_id is not set")]
    MissingZoneId,
    #[error("no CDN API token; set {0}")]
    MissingToken(String),
    #[error("building the HTTP client failed: {0}")]
    Client(#[source] reqwest::Error),
    #[error("the cache purge request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("the cache purge was rejected (HTTP {status}): {message}")]
    Api { status: u16, message: String },
}

/// What [`purge_all`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// No `cdn.provider` is configured; nothing was called.
    Disabled,
    /// The provider accepted the purge.
    Purged {
        provider: &'static str,
        /// The provider's own id for the purge, when it returns one.
        id: Option<String>,
    },
}

impl std::fmt::Display for PurgeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PurgeOutcome::Disabled => write!(f, "cdn purge disabled (no cdn.provider configured)"),
            PurgeOutcome::Purged { provider, id } => match id {
                Some(id) => write!(f, "purged the whole {provider} cache (id {id})"),
                None => write!(f, "purged the whole {provider} cache"),
            },
        }
    }
}

/// Purge the CDN's entire cache for the configured zone.
///
/// Returns [`PurgeOutcome::Disabled`] without touching the network when no
/// provider is configured, so callers need no `if enabled` of their own.
pub async fn purge_all(cfg: &Config) -> Result<PurgeOutcome, CdnError> {
    purge_all_at(cfg, CLOUDFLARE_API_BASE).await
}

/// [`purge_all`] against an explicit API root (tests point this at loopback).
pub async fn purge_all_at(cfg: &Config, api_base: &str) -> Result<PurgeOutcome, CdnError> {
    let Some(provider) = cfg.cdn.provider_name() else {
        return Ok(PurgeOutcome::Disabled);
    };
    if provider != CDN_CLOUDFLARE {
        return Err(CdnError::UnknownProvider(provider));
    }
    let zone = cfg.cdn.zone_id().ok_or(CdnError::MissingZoneId)?;
    let token = cfg
        .cdn
        .api_token()
        .ok_or_else(|| CdnError::MissingToken(crate::config::CdnConfig::api_token_env_var()))?;

    let client = build_client(PURGE_TIMEOUT).map_err(CdnError::Client)?;
    let url = format!(
        "{}/zones/{zone}/purge_cache",
        api_base.trim_end_matches('/')
    );
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(500),
        max_delay: Duration::from_secs(5),
    };
    let id = policy
        .run("cloudflare cache purge", is_retryable, || async {
            let response = client
                .post(&url)
                .bearer_auth(token)
                .json(&serde_json::json!({ "purge_everything": true }))
                .send()
                .await
                .map_err(CdnError::Request)?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            interpret(status.as_u16(), &body)
        })
        .await?;
    Ok(PurgeOutcome::Purged {
        provider: CDN_CLOUDFLARE,
        id,
    })
}

/// Cloudflare answers `{"success": …, "errors": [{"code", "message"}], "result": …}`.
/// A 200 with `success: false` is still a failure, so the status alone is not
/// enough to go on.
fn interpret(status: u16, body: &str) -> Result<Option<String>, CdnError> {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let success = json
        .as_ref()
        .and_then(|value| value.get("success"))
        .and_then(serde_json::Value::as_bool);
    if (200..300).contains(&status) && success != Some(false) {
        let id = json
            .as_ref()
            .and_then(|value| value.pointer("/result/id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        return Ok(id);
    }
    Err(CdnError::Api {
        status,
        message: api_message(json.as_ref(), body),
    })
}

/// The API's own `errors[].message` text, falling back to the raw body.
fn api_message(json: Option<&serde_json::Value>, body: &str) -> String {
    let messages: Vec<String> = json
        .and_then(|value| value.get("errors"))
        .and_then(serde_json::Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(|error| {
                    let message = error.get("message").and_then(serde_json::Value::as_str)?;
                    Some(
                        match error.get("code").and_then(serde_json::Value::as_i64) {
                            Some(code) => format!("{message} (code {code})"),
                            None => message.to_string(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !messages.is_empty() {
        return messages.join("; ");
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no response body".into()
    } else {
        trimmed.chars().take(300).collect()
    }
}

/// Retry transport failures and 5xx/429; a 4xx (bad token, wrong zone) is the
/// operator's problem and retrying only delays the message.
fn is_retryable(error: &CdnError) -> bool {
    match error {
        CdnError::Request(source) => crate::http::is_retryable(source),
        CdnError::Api { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use serde_json::json;

    use super::*;

    #[derive(Clone, Default)]
    struct Fake {
        scripted: Arc<Mutex<std::collections::VecDeque<(StatusCode, serde_json::Value)>>>,
        seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
    }

    impl Fake {
        fn push(&self, status: StatusCode, body: serde_json::Value) {
            self.scripted
                .lock()
                .expect("script lock")
                .push_back((status, body));
        }

        fn requests(&self) -> Vec<(HeaderMap, serde_json::Value)> {
            self.seen.lock().expect("seen lock").clone()
        }
    }

    async fn handle(
        State(fake): State<Fake>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> (StatusCode, axum::Json<serde_json::Value>) {
        fake.seen.lock().expect("seen lock").push((headers, body));
        let (status, body) = fake
            .scripted
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"success": false, "errors": [{"message": "unscripted"}]}),
            ));
        (status, axum::Json(body))
    }

    async fn serve(fake: Fake) -> String {
        let app = Router::new()
            .route("/zones/{zone}/purge_cache", post(handle))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn configured() -> Config {
        let mut config = Config::default();
        config.cdn.provider = Some("cloudflare".into());
        config.cdn.cloudflare_zone_id = Some("zone-abc".into());
        config.cdn.api_token = Some("token-never-logged".into());
        config
    }

    #[tokio::test]
    async fn no_provider_means_no_request() {
        let config = Config::default();
        assert!(!config.cdn.is_enabled());
        assert_eq!(
            purge_all_at(&config, "http://127.0.0.1:1/never-called")
                .await
                .unwrap(),
            PurgeOutcome::Disabled
        );
    }

    #[tokio::test]
    async fn a_successful_purge_sends_the_documented_request() {
        let fake = Fake::default();
        fake.push(
            StatusCode::OK,
            json!({"success": true, "errors": [], "messages": [], "result": {"id": "zone-abc"}}),
        );
        let base = serve(fake.clone()).await;

        let outcome = purge_all_at(&configured(), &base).await.expect("purge");
        assert_eq!(
            outcome,
            PurgeOutcome::Purged {
                provider: "cloudflare",
                id: Some("zone-abc".into()),
            }
        );

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer token-never-logged")
        );
        assert_eq!(body, &json!({"purge_everything": true}));
    }

    #[tokio::test]
    async fn a_success_false_body_is_an_error_carrying_the_api_message() {
        let fake = Fake::default();
        fake.push(
            StatusCode::OK,
            json!({
                "success": false,
                "errors": [{"code": 10000, "message": "Authentication error"}],
                "result": null
            }),
        );
        let base = serve(fake).await;
        let error = purge_all_at(&configured(), &base).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("Authentication error"), "{text}");
        assert!(text.contains("10000"), "{text}");
    }

    #[tokio::test]
    async fn a_4xx_is_not_retried_and_a_5xx_is() {
        let fake = Fake::default();
        fake.push(
            StatusCode::FORBIDDEN,
            json!({"success": false, "errors": [{"code": 9109, "message": "Invalid access token"}]}),
        );
        let base = serve(fake.clone()).await;
        let error = purge_all_at(&configured(), &base).await.unwrap_err();
        assert!(error.to_string().contains("Invalid access token"));
        assert_eq!(fake.requests().len(), 1, "a 403 must not be retried");

        let fake = Fake::default();
        fake.push(
            StatusCode::BAD_GATEWAY,
            json!({"success": false, "errors": [{"message": "bad gateway"}]}),
        );
        fake.push(
            StatusCode::OK,
            json!({"success": true, "errors": [], "result": {"id": "zone-abc"}}),
        );
        let base = serve(fake.clone()).await;
        purge_all_at(&configured(), &base)
            .await
            .expect("the retry succeeds");
        assert_eq!(fake.requests().len(), 2);
    }

    #[test]
    fn config_validation_demands_a_zone_and_a_token() {
        let mut config = Config::default();
        config.validate().expect("no cdn section is valid");

        config.cdn.provider = Some("cloudflare".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("cloudflare_zone_id"), "{error}");

        config.cdn.cloudflare_zone_id = Some("zone-abc".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("DAILY_EPUB_CDN__API_TOKEN"), "{error}");

        config.cdn.api_token = Some("token".into());
        config.validate().expect("fully configured");

        config.cdn.provider = Some("fastly".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("not recognised"), "{error}");
    }

    #[test]
    fn the_token_is_stripped_from_a_redacted_copy() {
        let config = configured();
        let redacted = config.cdn.redacted();
        assert!(redacted.api_token.is_none());
        assert_eq!(redacted.cloudflare_zone_id.as_deref(), Some("zone-abc"));
    }
}
