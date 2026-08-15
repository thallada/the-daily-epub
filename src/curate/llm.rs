//! DeepSeek client and token/cost accounting (spec §3.6).
//!
//! The OpenAI-compatible chat-completions endpoint at `https://api.deepseek.com/v1`.
//! DeepSeek prefix-caches automatically, so the (identical, long) taste-profile
//! system prompt must come first in every request: cached input is $0.0028/M vs
//! $0.14/M.
//!
//! **Why not `async-openai`** (spec §2 crate table): the published crate exposes
//! neither `Client` nor `types::chat` under any feature combination we could get
//! to build here, and it would drag in a second HTTP stack besides the shared
//! `reqwest` client (notes §4). [`DeepseekBackend`] therefore speaks the same
//! OpenAI-compatible wire protocol directly — about 80 lines, no new dependency,
//! and the request/response shapes are pinned by this module's tests. The
//! dependency was dropped from `Cargo.toml`; swapping a vendor SDK back in later
//! is a single [`ChatBackend`] impl and nothing else moves.
//!
//! Every call in the project goes through [`LlmClient`], which
//!
//! 1. always sends [`LlmClient::system_prompt`] as the **first** message, byte for
//!    byte identical across requests (that is what makes the prefix cache hit),
//! 2. folds the response's token usage into a shared [`UsageMeter`], and
//! 3. refuses further work once `max_daily_usd` has been spent (§3.6 guardrail).
//!
//! The network is reached through a [`ChatBackend`] so tests can inject canned
//! responses ([`MockBackend`]) without touching the wire (notes §6).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::json;

use crate::config::DeepseekConfig;
use crate::http::RetryPolicy;
use crate::types::TokenUsage;

/// `response_format` value used for every structured call (§3.6).
pub const JSON_OBJECT: &str = "json_object";

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("deepseek api key is not configured (set DAILY_EPUB_DEEPSEEK__API_KEY)")]
    MissingApiKey,
    #[error("deepseek request failed: {0}")]
    Api(String),
    /// A 5xx/429/network failure: worth retrying (crate table "retry").
    #[error("deepseek request failed (transient): {0}")]
    Transient(String),
    #[error("deepseek returned an empty completion")]
    EmptyResponse,
    #[error("deepseek returned unparseable JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The `max_daily_usd` ceiling was reached: callers must skip remaining
    /// editorial calls and fall back to feed excerpts, loudly (§3.6).
    #[error("daily cost ceiling of ${limit:.2} reached (spent ${spent:.4})")]
    BudgetExceeded { spent: f64, limit: f64 },
}

impl LlmError {
    /// True for failures the [`RetryPolicy`] should retry.
    pub fn is_transient(&self) -> bool {
        matches!(self, LlmError::Transient(_))
    }
}

// ---------------------------------------------------------------------------
// Usage metering (§3.6 cost guardrail, notes §5)
// ---------------------------------------------------------------------------

/// Shared token/cost accumulator enforcing `max_daily_usd` (notes §5).
///
/// Cloning shares the counters: one meter per run, cloned into every stage.
#[derive(Debug, Clone)]
pub struct UsageMeter {
    inner: Arc<Mutex<TokenUsage>>,
    /// Sticky: once the ceiling is crossed the run stays degraded (§3.6).
    exceeded: Arc<AtomicBool>,
    limit_usd: f64,
    price_input: f64,
    price_cached: f64,
    price_output: f64,
}

impl UsageMeter {
    pub fn new(cfg: &DeepseekConfig, limit_usd: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TokenUsage::default())),
            exceeded: Arc::new(AtomicBool::new(false)),
            limit_usd,
            price_input: cfg.price_input_per_mtok,
            price_cached: cfg.price_cached_input_per_mtok,
            price_output: cfg.price_output_per_mtok,
        }
    }

    /// Seed the meter with spend already recorded for the day (§3.6): the
    /// guardrail is a *daily* ceiling, not a per-run one.
    pub fn preload_cost(&self, spent_usd: f64) {
        if spent_usd > 0.0 && self.limit_usd > 0.0 && spent_usd >= self.limit_usd {
            self.trip("prior spend for today already exceeds the ceiling");
        }
    }

    /// Fold one response's usage in and return the running total.
    pub fn record(&self, usage: TokenUsage) -> TokenUsage {
        let total = match self.inner.lock() {
            Ok(mut guard) => {
                guard.add(usage);
                *guard
            }
            // A poisoned mutex must not abort a run: accounting is advisory.
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.add(usage);
                *guard
            }
        };
        let cost = self.cost_of(total);
        tracing::debug!(
            input = usage.input_tokens,
            cached = usage.cached_tokens,
            output = usage.output_tokens,
            total_cost_usd = cost,
            "recorded llm usage"
        );
        if self.limit_usd > 0.0 && cost > self.limit_usd && !self.exceeded.load(Ordering::SeqCst) {
            self.trip("token spend crossed the ceiling");
        }
        total
    }

    fn trip(&self, why: &str) {
        self.exceeded.store(true, Ordering::SeqCst);
        tracing::error!(
            spent_usd = self.cost_usd(),
            limit_usd = self.limit_usd,
            "LLM budget exceeded ({why}): remaining editorial calls will be skipped \
             and feed excerpts used instead"
        );
    }

    pub fn total(&self) -> TokenUsage {
        match self.inner.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn cost_of(&self, usage: TokenUsage) -> f64 {
        usage.cost_usd(self.price_input, self.price_cached, self.price_output)
    }

    pub fn cost_usd(&self) -> f64 {
        self.cost_of(self.total())
    }

    pub fn limit_usd(&self) -> f64 {
        self.limit_usd
    }

    /// True once the ceiling has been crossed — editorial stages check this and
    /// silently degrade to excerpts (§3.6).
    pub fn budget_exceeded(&self) -> bool {
        self.exceeded.load(Ordering::SeqCst)
    }

    /// `Err(BudgetExceeded)` once the run has spent more than `max_daily_usd` (§3.6).
    pub fn check_budget(&self) -> Result<(), LlmError> {
        if self.budget_exceeded() {
            return Err(LlmError::BudgetExceeded {
                spent: self.cost_usd(),
                limit: self.limit_usd,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backend abstraction (notes §6: no network in tests)
// ---------------------------------------------------------------------------

/// One chat completion request. The system prompt is an [`Arc`] so that the
/// identical bytes are reused for every call (DeepSeek prefix caching, §3.6).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: Arc<String>,
    pub user: String,
    pub temperature: f32,
    /// Ask for `response_format: {"type": "json_object"}` (§3.6).
    pub json: bool,
}

/// One chat completion response, reduced to what the pipeline needs.
#[derive(Debug, Clone, Default)]
pub struct ChatCompletion {
    pub content: String,
    pub usage: TokenUsage,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The seam between [`LlmClient`] and the network (notes §6).
pub trait ChatBackend: std::fmt::Debug + Send + Sync {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>>;
}

/// LLM calls are slow; the shared 10s HTTP timeout would kill them (notes §4).
const LLM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// The real thing: the OpenAI-compatible endpoint at `deepseek.base_url` (§3.6).
#[derive(Debug, Clone)]
pub struct DeepseekBackend {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl DeepseekBackend {
    pub fn new(cfg: &DeepseekConfig) -> Result<Self, LlmError> {
        let api_key = cfg
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or(LlmError::MissingApiKey)?
            .to_string();
        let http = crate::http::build_client(LLM_TIMEOUT)
            .map_err(|e| LlmError::Api(format!("building the deepseek http client: {e}")))?;
        Ok(Self {
            http,
            endpoint: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            api_key,
        })
    }
}

impl ChatBackend for DeepseekBackend {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>> {
        Box::pin(async move {
            let mut body = json!({
                "model": req.model,
                "messages": [
                    // FIRST and byte-identical across every request: prefix cache (§3.6).
                    {"role": "system", "content": req.system.as_str()},
                    {"role": "user", "content": req.user},
                ],
                "temperature": req.temperature,
                "stream": false,
            });
            if req.json
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert("response_format".into(), json!({"type": JSON_OBJECT}));
            }

            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(classify_reqwest_error)?;

            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail = detail.chars().take(500).collect::<String>();
                let msg = format!("{status}: {detail}");
                return Err(if status.is_server_error() || status.as_u16() == 429 {
                    LlmError::Transient(msg)
                } else {
                    LlmError::Api(msg)
                });
            }

            let parsed: ApiResponse = response.json().await.map_err(|e| {
                LlmError::Api(format!("decoding the deepseek chat completion: {e}"))
            })?;
            let content = parsed
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .filter(|c| !c.trim().is_empty())
                .ok_or(LlmError::EmptyResponse)?;
            let usage = parsed.usage.map(usage_from_api).unwrap_or_default();
            Ok(ChatCompletion { content, usage })
        })
    }
}

/// The slice of the chat-completions response we consume.
#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    choices: Vec<ApiChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct ApiChoice {
    message: ApiMessage,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(default)]
    content: Option<String>,
}

/// DeepSeek reports cache hits both OpenAI-style (`prompt_tokens_details`) and
/// natively (`prompt_cache_hit_tokens`); we accept either (§3.6 pricing).
#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<i64>,
    #[serde(default)]
    prompt_tokens_details: Option<ApiPromptTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

/// Split `prompt_tokens` into cache-miss and cache-hit halves (§3.6 pricing).
fn usage_from_api(u: ApiUsage) -> TokenUsage {
    let cached = u
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .or(u.prompt_cache_hit_tokens)
        .unwrap_or(0)
        .max(0);
    let prompt = u.prompt_tokens.max(0);
    let cached = cached.min(prompt);
    TokenUsage {
        input_tokens: prompt - cached,
        cached_tokens: cached,
        output_tokens: u.completion_tokens.max(0),
    }
}

fn classify_reqwest_error(err: reqwest::Error) -> LlmError {
    if crate::http::is_retryable(&err) {
        LlmError::Transient(err.to_string())
    } else {
        LlmError::Api(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Every LLM call in the project goes through this client (notes §5).
#[derive(Debug, Clone)]
pub struct LlmClient {
    /// The taste profile, sent as the first (cacheable) system message (§3.6).
    pub system_prompt: Arc<String>,
    pub model: String,
    pub meter: UsageMeter,
    backend: Arc<dyn ChatBackend>,
    retry: RetryPolicy,
}

impl LlmClient {
    /// Build against the configured base URL; fails without an API key.
    pub fn new(
        cfg: &DeepseekConfig,
        system_prompt: String,
        meter: UsageMeter,
    ) -> Result<Self, LlmError> {
        let backend = DeepseekBackend::new(cfg)?;
        tracing::debug!(
            base_url = %cfg.base_url,
            model = %cfg.model,
            system_prompt_chars = system_prompt.len(),
            "deepseek client ready"
        );
        Ok(Self::with_backend(
            &cfg.model,
            system_prompt,
            meter,
            Arc::new(backend),
        ))
    }

    /// Construct around an arbitrary backend — the seam used by tests (notes §6).
    pub fn with_backend(
        model: &str,
        system_prompt: String,
        meter: UsageMeter,
        backend: Arc<dyn ChatBackend>,
    ) -> Self {
        Self {
            system_prompt: Arc::new(system_prompt),
            model: model.to_string(),
            meter,
            backend,
            retry: RetryPolicy::default(),
        }
    }

    /// Raw completion: budget check → retry loop → usage accounting.
    pub async fn complete(
        &self,
        user_prompt: &str,
        temperature: f32,
        json: bool,
    ) -> Result<String, LlmError> {
        self.meter.check_budget()?;
        let req = ChatRequest {
            model: self.model.clone(),
            system: Arc::clone(&self.system_prompt),
            user: user_prompt.to_string(),
            temperature,
            json,
        };
        let completion = self
            .retry
            .run("deepseek chat completion", LlmError::is_transient, || {
                self.backend.complete(req.clone())
            })
            .await?;
        self.meter.record(completion.usage);
        Ok(completion.content)
    }

    /// One chat completion returning parsed JSON of type `T`, with the system
    /// prompt first and `response_format: json_object` (§3.6).
    pub async fn complete_json<T: serde::de::DeserializeOwned>(
        &self,
        user_prompt: &str,
        temperature: f32,
    ) -> Result<T, LlmError> {
        let raw = self.complete(user_prompt, temperature, true).await?;
        let cleaned = strip_code_fence(&raw);
        match serde_json::from_str::<T>(cleaned) {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    preview = %cleaned.chars().take(400).collect::<String>(),
                    "deepseek returned malformed JSON"
                );
                Err(LlmError::Json(e))
            }
        }
    }

    /// One plain-text completion (used for the front page / intros) (§3.6).
    pub async fn complete_text(
        &self,
        user_prompt: &str,
        temperature: f32,
    ) -> Result<String, LlmError> {
        self.complete(user_prompt, temperature, false).await
    }
}

/// Models occasionally wrap JSON in ```` ```json ```` fences despite `json_object`.
pub fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start_matches(['\n', '\r'])
        .trim_end()
        .trim_end_matches("```")
        .trim()
}

// ---------------------------------------------------------------------------
// Test backend
// ---------------------------------------------------------------------------

/// Canned-response backend for tests: pops scripted replies in order (notes §6).
#[derive(Debug, Default)]
pub struct MockBackend {
    scripted: Mutex<std::collections::VecDeque<Result<ChatCompletion, String>>>,
    /// Every prompt the code under test sent, in order.
    pub seen: Mutex<Vec<ChatRequest>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a successful reply carrying `usage` tokens.
    pub fn push(&self, content: impl Into<String>, usage: TokenUsage) {
        if let Ok(mut q) = self.scripted.lock() {
            q.push_back(Ok(ChatCompletion {
                content: content.into(),
                usage,
            }));
        }
    }

    /// Queue a permanent (non-retryable) failure.
    pub fn push_error(&self, message: impl Into<String>) {
        if let Ok(mut q) = self.scripted.lock() {
            q.push_back(Err(message.into()));
        }
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().map(|s| s.len()).unwrap_or(0)
    }

    pub fn prompts(&self) -> Vec<ChatRequest> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl ChatBackend for MockBackend {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>> {
        Box::pin(async move {
            let next = self.scripted.lock().ok().and_then(|mut q| q.pop_front());
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(req);
            }
            match next {
                Some(Ok(c)) => Ok(c),
                Some(Err(msg)) => Err(LlmError::Api(msg)),
                None => Err(LlmError::Api("mock backend ran out of responses".into())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DeepseekConfig {
        DeepseekConfig::default()
    }

    pub(crate) fn tokens(input: i64, cached: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_tokens: cached,
            output_tokens: output,
        }
    }

    #[test]
    fn meter_accumulates_and_prices() {
        let meter = UsageMeter::new(&cfg(), 2.0);
        meter.record(tokens(1_000_000, 0, 0));
        meter.record(tokens(0, 1_000_000, 1_000_000));
        let total = meter.total();
        assert_eq!(total.input_tokens, 1_000_000);
        assert_eq!(total.cached_tokens, 1_000_000);
        assert_eq!(total.output_tokens, 1_000_000);
        assert!((meter.cost_usd() - 0.4228).abs() < 1e-9);
        assert!(!meter.budget_exceeded());
        assert!(meter.check_budget().is_ok());
    }

    #[test]
    fn meter_trips_the_budget_flag_and_stays_tripped() {
        // Ceiling of $0.10; 1M cache-miss input tokens costs $0.14.
        let meter = UsageMeter::new(&cfg(), 0.10);
        meter.record(tokens(1_000_000, 0, 0));
        assert!(meter.budget_exceeded());
        assert!(matches!(
            meter.check_budget(),
            Err(LlmError::BudgetExceeded { .. })
        ));
        // Cloned meters share the flag.
        assert!(meter.clone().budget_exceeded());
    }

    #[test]
    fn preloaded_daily_spend_trips_the_flag() {
        let meter = UsageMeter::new(&cfg(), 1.0);
        meter.preload_cost(0.5);
        assert!(!meter.budget_exceeded());
        meter.preload_cost(1.5);
        assert!(meter.budget_exceeded());
    }

    #[test]
    fn usage_split_uses_prompt_token_details() {
        let u: ApiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 1000, "completion_tokens": 120, "total_tokens": 1120,
                "prompt_tokens_details": {"cached_tokens": 800}}"#,
        )
        .expect("fixture usage");
        assert_eq!(usage_from_api(u), tokens(200, 800, 120));
    }

    #[test]
    fn usage_falls_back_to_deepseek_native_cache_fields() {
        let u: ApiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 500, "completion_tokens": 40,
                "prompt_cache_hit_tokens": 448, "prompt_cache_miss_tokens": 52}"#,
        )
        .expect("fixture usage");
        assert_eq!(usage_from_api(u), tokens(52, 448, 40));
        // Missing usage is not an error, just zero.
        let empty: ApiUsage = serde_json::from_str("{}").expect("empty usage");
        assert_eq!(usage_from_api(empty), TokenUsage::default());
    }

    #[test]
    fn code_fences_are_stripped() {
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    fn client(backend: Arc<MockBackend>, limit: f64) -> LlmClient {
        LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM PROMPT".into(),
            UsageMeter::new(&cfg(), limit),
            backend,
        )
    }

    #[tokio::test]
    async fn json_completion_records_usage_and_sends_system_prompt_first() {
        let backend = Arc::new(MockBackend::new());
        backend.push(r#"{"value": 42}"#, tokens(10, 90, 5));
        let llm = client(Arc::clone(&backend), 2.0);

        #[derive(serde::Deserialize)]
        struct Out {
            value: i64,
        }
        let out: Out = llm
            .complete_json("score these", 0.3)
            .await
            .expect("mock completion");
        assert_eq!(out.value, 42);
        assert_eq!(llm.meter.total(), tokens(10, 90, 5));

        let prompts = backend.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].system.as_str(), "SYSTEM PROMPT");
        assert!(prompts[0].json);
        assert_eq!(prompts[0].user, "score these");
    }

    #[tokio::test]
    async fn identical_system_prompt_bytes_across_calls() {
        let backend = Arc::new(MockBackend::new());
        backend.push("{}", TokenUsage::default());
        backend.push("{}", TokenUsage::default());
        let llm = client(Arc::clone(&backend), 2.0);
        let _: serde_json::Value = llm.complete_json("a", 0.3).await.expect("first");
        let _: serde_json::Value = llm.complete_json("b", 0.3).await.expect("second");
        let prompts = backend.prompts();
        assert_eq!(prompts[0].system.as_bytes(), prompts[1].system.as_bytes());
    }

    #[tokio::test]
    async fn calls_are_refused_once_the_budget_is_gone() {
        let backend = Arc::new(MockBackend::new());
        backend.push("{}", tokens(1_000_000, 0, 0));
        let llm = client(Arc::clone(&backend), 0.01);
        let _: serde_json::Value = llm.complete_json("first", 0.3).await.expect("first call");
        let err = llm
            .complete_text("second", 0.3)
            .await
            .expect_err("budget must be enforced");
        assert!(matches!(err, LlmError::BudgetExceeded { .. }));
        // The refused call never reached the backend.
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn malformed_json_surfaces_as_json_error() {
        let backend = Arc::new(MockBackend::new());
        backend.push("not json at all", TokenUsage::default());
        let llm = client(backend, 2.0);
        let out: Result<serde_json::Value, _> = llm.complete_json("x", 0.3).await;
        assert!(matches!(out, Err(LlmError::Json(_))));
    }

    #[test]
    fn missing_api_key_is_reported() {
        let cfg = DeepseekConfig {
            api_key: Some("   ".into()),
            ..DeepseekConfig::default()
        };
        assert!(matches!(
            DeepseekBackend::new(&cfg),
            Err(LlmError::MissingApiKey)
        ));
    }
}
