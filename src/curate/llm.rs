//! Provider-neutral LLM clients, transports, retry, and token accounting (§4, §5).
//!
//! Two transports speak to the wire directly through the shared `reqwest`
//! client (no vendor SDK; implementation notes, cross-cutting item 5):
//!
//! - [`OpenAiCompatibleBackend`]: `POST {base_url}/chat/completions` with a
//!   bearer key — DeepSeek, Gemini's compatibility endpoint, OpenAI, a local
//!   server. The system prompt is the first message so prefix caches hit;
//!   `reasoning_effort` is sent when the provider configures an `effort`.
//! - [`AnthropicBackend`]: `POST {base_url}/v1/messages` with the system prompt
//!   as one `cache_control: ephemeral` block, `output_config.effort`, and
//!   server-side `fallbacks: "default"` (§4.2). No sampling parameters, no
//!   `thinking`, no prefill — Opus 5 rejects them. A `stop_reason: "refusal"`
//!   (HTTP 200) is [`LlmError::Refusal`], which the callers use to fall back to
//!   the bulk client.
//!
//! Which transport a role uses is decided by name: `[llm] bulk = "deepseek"`
//! looks up `[providers.deepseek]` and its `kind`. Every call goes through
//! [`LlmClient`], which sends the byte-identical system prompt on every request,
//! folds token usage into that provider's [`UsageMeter`] priced by its
//! [`PriceTable`], and refuses further work once its `max_daily_usd` is spent.
//! [`Llms`] pairs the bulk and editor clients.
//!
//! Tests inject [`MockBackend`] or a loopback `axum` listener; nothing here
//! touches the network under test.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::json;

use crate::config::{Config, ProviderConfig, ProviderKind};
use crate::http::RetryPolicy;
use crate::types::TokenUsage;

pub const JSON_OBJECT: &str = "json_object";
/// Concurrency for clients built without a provider entry (tests, mocks).
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 4;
const OPENAI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const ANTHROPIC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{provider} api key is not configured (set {env_var})")]
    MissingApiKey { provider: String, env_var: String },
    #[error("{provider} request failed: {message}")]
    Api { provider: String, message: String },
    #[error("{provider} request failed (transient): {message}")]
    Transient { provider: String, message: String },
    #[error("{provider} returned a refusal")]
    Refusal { provider: String },
    #[error("{provider} returned an empty completion")]
    EmptyResponse { provider: String },
    #[error("llm returned unparseable JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daily cost ceiling of ${limit:.2} reached (spent ${spent:.4})")]
    BudgetExceeded { spent: f64, limit: f64 },
}

impl LlmError {
    pub fn is_transient(&self) -> bool {
        matches!(self, LlmError::Transient { .. })
    }

    pub fn api(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Api {
            provider: provider.into(),
            message: message.into(),
        }
    }

    fn transient(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Transient {
            provider: provider.into(),
            message: message.into(),
        }
    }

    pub fn refusal(provider: impl Into<String>) -> Self {
        Self::Refusal {
            provider: provider.into(),
        }
    }

    pub fn empty_response(provider: impl Into<String>) -> Self {
        Self::EmptyResponse {
            provider: provider.into(),
        }
    }

    fn missing_api_key(provider: &str) -> Self {
        Self::MissingApiKey {
            provider: provider.to_string(),
            env_var: ProviderConfig::api_key_env_var(provider),
        }
    }
}

/// USD per 1M tokens for the four counters of [`TokenUsage`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceTable {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl From<&ProviderConfig> for PriceTable {
    fn from(cfg: &ProviderConfig) -> Self {
        Self {
            input: cfg.price_input_per_mtok,
            cache_read: cfg.price_cache_read_per_mtok,
            cache_write: cfg.price_cache_write_per_mtok,
            output: cfg.price_output_per_mtok,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UsageMeter {
    inner: Arc<Mutex<TokenUsage>>,
    exceeded: Arc<AtomicBool>,
    prior_spend_usd: Arc<Mutex<f64>>,
    limit_usd: f64,
    prices: PriceTable,
}

impl UsageMeter {
    /// A meter priced from a `[providers.<name>]` entry with its `max_daily_usd`.
    pub fn for_provider(cfg: &ProviderConfig) -> Self {
        Self::with_prices(PriceTable::from(cfg), cfg.max_daily_usd)
    }

    pub fn with_prices(prices: PriceTable, limit_usd: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TokenUsage::default())),
            exceeded: Arc::new(AtomicBool::new(false)),
            prior_spend_usd: Arc::new(Mutex::new(0.0)),
            limit_usd,
            prices,
        }
    }

    pub fn preload_cost(&self, spent_usd: f64) {
        let spent_usd = spent_usd.max(0.0);
        match self.prior_spend_usd.lock() {
            Ok(mut guard) => *guard = spent_usd,
            Err(poisoned) => *poisoned.into_inner() = spent_usd,
        }
        if self.limit_usd > 0.0 && spent_usd >= self.limit_usd {
            self.trip("prior spend for the run's UTC day reached the ceiling");
        }
    }

    pub fn record(&self, usage: TokenUsage) -> TokenUsage {
        let total = match self.inner.lock() {
            Ok(mut guard) => {
                guard.add(usage);
                *guard
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.add(usage);
                *guard
            }
        };
        let spent = self.spent_usd();
        tracing::debug!(
            input = usage.input_tokens,
            cache_write = usage.cache_write_tokens,
            cache_read = usage.cached_tokens,
            output = usage.output_tokens,
            live_cost_usd = self.cost_usd(),
            day_spend_usd = spent,
            "recorded llm usage"
        );
        if self.limit_usd > 0.0 && spent > self.limit_usd {
            self.trip("token spend crossed the ceiling");
        }
        total
    }

    fn trip(&self, why: &str) {
        self.exceeded.store(true, Ordering::SeqCst);
        tracing::error!(
            spent_usd = self.spent_usd(),
            limit_usd = self.limit_usd,
            "LLM budget exceeded ({why}); remaining calls for this provider are skipped"
        );
    }

    pub fn total(&self) -> TokenUsage {
        match self.inner.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub fn cost_of(&self, usage: TokenUsage) -> f64 {
        usage.cost_usd(
            self.prices.input,
            self.prices.cache_write,
            self.prices.cache_read,
            self.prices.output,
        )
    }

    pub fn cost_usd(&self) -> f64 {
        self.cost_of(self.total())
    }

    pub fn spent_usd(&self) -> f64 {
        let prior = match self.prior_spend_usd.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        prior + self.cost_usd()
    }

    pub fn limit_usd(&self) -> f64 {
        self.limit_usd
    }

    pub fn budget_exceeded(&self) -> bool {
        self.exceeded.load(Ordering::SeqCst)
    }

    pub fn check_budget(&self) -> Result<(), LlmError> {
        if self.budget_exceeded() || (self.limit_usd > 0.0 && self.spent_usd() >= self.limit_usd) {
            self.exceeded.store(true, Ordering::SeqCst);
            return Err(LlmError::BudgetExceeded {
                spent: self.spent_usd(),
                limit: self.limit_usd,
            });
        }
        Ok(())
    }
}

/// One [`UsageMeter`] per provider that an `[llm]` role references, keyed by
/// provider name. Two roles on one provider share one meter and one ceiling.
pub fn provider_meters(config: &Config) -> BTreeMap<String, UsageMeter> {
    config
        .referenced_providers()
        .into_iter()
        .map(|(name, provider)| (name.to_string(), UsageMeter::for_provider(provider)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: Arc<String>,
    pub user: String,
    pub temperature: f32,
    pub json: bool,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletion {
    pub content: String,
    pub usage: TokenUsage,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ChatBackend: std::fmt::Debug + Send + Sync {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>>;
}

/// The OpenAI-compatible chat-completions transport (`kind = "openai"`).
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleBackend {
    provider: Arc<str>,
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl OpenAiCompatibleBackend {
    /// `name` is the `[providers.<name>]` key; it labels errors and log lines.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, LlmError> {
        let api_key = cfg
            .api_key()
            .ok_or_else(|| LlmError::missing_api_key(name))?
            .to_string();
        let http = crate::http::build_client(OPENAI_TIMEOUT)
            .map_err(|error| LlmError::api(name, format!("building http client: {error}")))?;
        Ok(Self {
            provider: Arc::from(name),
            http,
            endpoint: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            api_key,
        })
    }
}

impl ChatBackend for OpenAiCompatibleBackend {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>> {
        Box::pin(async move {
            let provider = &*self.provider;
            let mut body = json!({
                "model": req.model,
                "messages": [
                    {"role": "system", "content": req.system.as_str()},
                    {"role": "user", "content": req.user},
                ],
                "temperature": req.temperature,
                "stream": false,
            });
            if let Some(object) = body.as_object_mut() {
                if req.json {
                    object.insert("response_format".into(), json!({"type": JSON_OBJECT}));
                }
                if let Some(effort) = req
                    .effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                {
                    object.insert("reasoning_effort".into(), json!(effort));
                }
            }
            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|error| classify_reqwest_error(provider, error))?;
            let status = response.status();
            if !status.is_success() {
                return Err(classify_status(provider, status, response).await);
            }
            let parsed: OpenAiResponse = response.json().await.map_err(|error| {
                LlmError::api(provider, format!("decoding chat completion: {error}"))
            })?;
            let content = parsed
                .choices
                .into_iter()
                .next()
                .and_then(|choice| choice.message.content)
                .filter(|content| !content.trim().is_empty())
                .ok_or_else(|| LlmError::empty_response(provider))?;
            let usage = parsed.usage.map(openai_usage).unwrap_or_default();
            Ok(ChatCompletion { content, usage })
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    #[serde(default)]
    content: Option<String>,
}

/// The `usage` object. `completion_tokens` already includes reasoning tokens
/// on every provider that reports `completion_tokens_details.reasoning_tokens`
/// (OpenAI, Gemini), so that detail is deliberately not added on top.
#[derive(Debug, Default, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    /// DeepSeek's native cache counter, the fallback for `cached_tokens`.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<i64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

fn openai_usage(usage: OpenAiUsage) -> TokenUsage {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .or(usage.prompt_cache_hit_tokens)
        .unwrap_or(0)
        .max(0);
    let prompt = usage.prompt_tokens.max(0);
    let cached = cached.min(prompt);
    TokenUsage {
        input_tokens: prompt - cached,
        cached_tokens: cached,
        cache_write_tokens: 0,
        output_tokens: usage.completion_tokens.max(0),
    }
}

/// The Anthropic Messages API transport (`kind = "anthropic"`).
#[derive(Debug, Clone)]
pub struct AnthropicBackend {
    provider: Arc<str>,
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl AnthropicBackend {
    /// `name` is the `[providers.<name>]` key; it labels errors and log lines.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, LlmError> {
        let api_key = cfg
            .api_key()
            .ok_or_else(|| LlmError::missing_api_key(name))?
            .to_string();
        let http = crate::http::build_client(ANTHROPIC_TIMEOUT)
            .map_err(|error| LlmError::api(name, format!("building http client: {error}")))?;
        Ok(Self {
            provider: Arc::from(name),
            http,
            endpoint: format!("{}/v1/messages", cfg.base_url.trim_end_matches('/')),
            api_key,
        })
    }
}

impl ChatBackend for AnthropicBackend {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>> {
        Box::pin(async move {
            let provider = &*self.provider;
            let body = json!({
                "model": req.model,
                "max_tokens": 16_000,
                "system": [{
                    "type": "text",
                    "text": req.system.as_str(),
                    "cache_control": {"type": "ephemeral"},
                }],
                "messages": [{"role": "user", "content": req.user}],
                "output_config": {"effort": req.effort.as_deref().unwrap_or("high")},
                "fallbacks": "default",
            });
            let response = self
                .http
                .post(&self.endpoint)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", ANTHROPIC_BETA)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| classify_reqwest_error(provider, error))?;
            let status = response.status();
            if !status.is_success() {
                return Err(classify_status(provider, status, response).await);
            }
            let parsed: AnthropicResponse = response.json().await.map_err(|error| {
                LlmError::api(provider, format!("decoding messages response: {error}"))
            })?;
            if parsed.stop_reason.as_deref() == Some("refusal") {
                return Err(LlmError::refusal(provider));
            }
            let content = parsed
                .content
                .into_iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text)
                .collect::<Vec<_>>()
                .join("");
            if content.trim().is_empty() {
                return Err(LlmError::empty_response(provider));
            }
            Ok(ChatCompletion {
                content,
                usage: anthropic_usage(parsed.usage),
            })
        })
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

fn anthropic_usage(usage: AnthropicUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens.max(0),
        cached_tokens: usage.cache_read_input_tokens.max(0),
        cache_write_tokens: usage.cache_creation_input_tokens.max(0),
        output_tokens: usage.output_tokens.max(0),
    }
}

async fn classify_status(
    provider: &str,
    status: reqwest::StatusCode,
    response: reqwest::Response,
) -> LlmError {
    let detail = response.text().await.unwrap_or_default();
    let message = format!("{status}: {}", detail.chars().take(500).collect::<String>());
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        LlmError::transient(provider, message)
    } else {
        LlmError::api(provider, message)
    }
}

fn classify_reqwest_error(provider: &str, error: reqwest::Error) -> LlmError {
    if crate::http::is_retryable(&error) {
        LlmError::transient(provider, error.to_string())
    } else {
        LlmError::api(provider, error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct LlmClient {
    /// The `[providers.<name>]` key: the `provider_costs` key, the meter's
    /// preload key and the label on every log line. Never the kind.
    pub provider: Arc<str>,
    pub system_prompt: Arc<String>,
    pub model: String,
    pub effort: Option<String>,
    /// Batches in flight for the stages that fan out on this client.
    pub max_concurrent_requests: usize,
    pub meter: UsageMeter,
    backend: Arc<dyn ChatBackend>,
    retry: RetryPolicy,
}

impl LlmClient {
    /// A client for one `[providers.<name>]` entry, dispatching on its `kind`.
    pub fn for_provider(
        name: &str,
        cfg: &ProviderConfig,
        system_prompt: String,
        meter: UsageMeter,
    ) -> Result<Self, LlmError> {
        let backend: Arc<dyn ChatBackend> = match cfg.kind {
            ProviderKind::OpenAi => Arc::new(OpenAiCompatibleBackend::new(name, cfg)?),
            ProviderKind::Anthropic => Arc::new(AnthropicBackend::new(name, cfg)?),
        };
        let mut client = Self::with_backend_options(
            name,
            &cfg.model,
            system_prompt,
            cfg.effort.clone(),
            meter,
            backend,
        );
        client.max_concurrent_requests = cfg.max_concurrent_requests.max(1);
        Ok(client)
    }

    pub fn with_backend(
        model: &str,
        system_prompt: String,
        meter: UsageMeter,
        backend: Arc<dyn ChatBackend>,
    ) -> Self {
        Self::with_backend_options("mock", model, system_prompt, None, meter, backend)
    }

    pub fn with_backend_options(
        provider: &str,
        model: &str,
        system_prompt: String,
        effort: Option<String>,
        meter: UsageMeter,
        backend: Arc<dyn ChatBackend>,
    ) -> Self {
        Self {
            provider: Arc::from(provider),
            system_prompt: Arc::new(system_prompt),
            model: model.to_string(),
            effort,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            meter,
            backend,
            retry: RetryPolicy::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The provider name as a plain `&str`.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub async fn complete(
        &self,
        user_prompt: &str,
        temperature: f32,
        json: bool,
    ) -> Result<String, LlmError> {
        self.meter.check_budget()?;
        let request = ChatRequest {
            model: self.model.clone(),
            system: Arc::clone(&self.system_prompt),
            user: user_prompt.to_string(),
            temperature,
            json,
            effort: self.effort.clone(),
        };
        let completion = self
            .retry
            .run(
                &format!("{} chat completion", self.provider),
                LlmError::is_transient,
                || self.backend.complete(request.clone()),
            )
            .await?;
        self.meter.record(completion.usage);
        Ok(completion.content)
    }

    pub async fn complete_json<T: serde::de::DeserializeOwned>(
        &self,
        user_prompt: &str,
        temperature: f32,
    ) -> Result<T, LlmError> {
        let raw = self.complete(user_prompt, temperature, true).await?;
        let cleaned = strip_code_fence(&raw);
        match serde_json::from_str(cleaned) {
            Ok(value) => Ok(value),
            Err(error) => {
                tracing::warn!(
                    provider = %self.provider,
                    %error,
                    preview = %cleaned.chars().take(400).collect::<String>(),
                    "llm returned malformed JSON"
                );
                Err(LlmError::Json(error))
            }
        }
    }

    pub async fn complete_text(
        &self,
        user_prompt: &str,
        temperature: f32,
    ) -> Result<String, LlmError> {
        self.complete(user_prompt, temperature, false).await
    }
}

/// The two role clients the pipeline works with (§4.2).
///
/// Both share the exact same system prompt string (§8.4). Each has the
/// [`UsageMeter`] of its provider, with that provider's price table and
/// `max_daily_usd` (§5); two roles on one provider share one meter.
#[derive(Debug, Clone, Default)]
pub struct Llms {
    /// `[llm] bulk` — triage, deep assessment, and the fallback for every editor call.
    pub bulk: Option<LlmClient>,
    /// `[llm] editor` — selection, summaries, the brief, the profile rebuild.
    pub editor: Option<LlmClient>,
}

impl Llms {
    /// Build both role clients by provider name with one shared system prompt.
    ///
    /// `meters` holds one meter per referenced provider (see
    /// [`provider_meters`]); a provider missing from it gets a fresh meter. A
    /// missing key or an unassigned role leaves that slot `None` with a log
    /// line naming the provider; nothing here is fatal because the paper
    /// always publishes (§17). When both roles name the same provider they
    /// share one client and so one meter.
    pub fn from_config(
        config: &Config,
        system_prompt: String,
        meters: &BTreeMap<String, UsageMeter>,
    ) -> Self {
        let build = |role: &str, name: &str, cfg: &ProviderConfig| {
            let meter = meters
                .get(name)
                .cloned()
                .unwrap_or_else(|| UsageMeter::for_provider(cfg));
            match LlmClient::for_provider(name, cfg, system_prompt.clone(), meter) {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!(role, provider = name, %error, "provider is unavailable");
                    None
                }
            }
        };
        let bulk = match config.bulk_provider() {
            Some((name, cfg)) => build("bulk", name, cfg),
            None => {
                tracing::info!("no bulk provider: triage and deep assessment are skipped");
                None
            }
        };
        let editor = match config.editor_provider() {
            Some((name, _)) if config.llm.bulk_name() == Some(name) => {
                tracing::info!(
                    provider = name,
                    "editor and bulk share one provider, client and ceiling"
                );
                bulk.clone()
            }
            Some((name, cfg)) => build("editor", name, cfg),
            None => {
                tracing::info!("no editor provider; editor work runs on the bulk provider");
                None
            }
        };
        Self { bulk, editor }
    }

    /// The editor when configured and its meter is not tripped, else bulk.
    pub fn editor_or_bulk(&self) -> Option<&LlmClient> {
        self.editor
            .as_ref()
            .filter(|client| !client.meter.budget_exceeded())
            .or(self.bulk.as_ref())
    }

    /// True when no provider at all is available (`--skip-llm` or no keys).
    pub fn is_empty(&self) -> bool {
        self.bulk.is_none() && self.editor.is_none()
    }
}

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

#[derive(Debug, Default)]
pub struct MockBackend {
    scripted: Mutex<std::collections::VecDeque<Result<ChatCompletion, LlmError>>>,
    pub seen: Mutex<Vec<ChatRequest>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, content: impl Into<String>, usage: TokenUsage) {
        if let Ok(mut queue) = self.scripted.lock() {
            queue.push_back(Ok(ChatCompletion {
                content: content.into(),
                usage,
            }));
        }
    }

    pub fn push_error(&self, message: impl Into<String>) {
        self.push_llm_error(LlmError::api("mock", message));
    }

    pub fn push_llm_error(&self, error: LlmError) {
        if let Ok(mut queue) = self.scripted.lock() {
            queue.push_back(Err(error));
        }
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().map(|seen| seen.len()).unwrap_or(0)
    }

    pub fn prompts(&self) -> Vec<ChatRequest> {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

impl ChatBackend for MockBackend {
    fn complete<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<ChatCompletion, LlmError>> {
        Box::pin(async move {
            let next = self
                .scripted
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front());
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(req);
            }
            next.unwrap_or_else(|| Err(LlmError::api("mock", "mock backend ran out of responses")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};

    fn deepseek() -> ProviderConfig {
        ProviderConfig::deepseek()
    }

    fn anthropic() -> ProviderConfig {
        ProviderConfig::anthropic()
    }

    fn deepseek_meter(limit: f64) -> UsageMeter {
        UsageMeter::with_prices(PriceTable::from(&deepseek()), limit)
    }

    pub(crate) fn tokens(input: i64, cached: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_tokens: cached,
            cache_write_tokens: 0,
            output_tokens: output,
        }
    }

    // -----------------------------------------------------------------------
    // Meter and pricing
    // -----------------------------------------------------------------------

    #[test]
    fn meter_accumulates_and_prices() {
        let meter = UsageMeter::for_provider(&deepseek());
        assert_eq!(meter.limit_usd(), 2.0, "the provider's own ceiling");
        meter.record(tokens(1_000_000, 0, 0));
        meter.record(tokens(0, 1_000_000, 1_000_000));
        let total = meter.total();
        assert_eq!(total.input_tokens, 1_000_000);
        assert_eq!(total.cached_tokens, 1_000_000);
        assert_eq!(total.output_tokens, 1_000_000);
        // The DeepSeek path prices exactly as before the cache-write counter.
        assert!((meter.cost_usd() - 0.4228).abs() < 1e-9);
        assert!(!meter.budget_exceeded());
        assert!(meter.check_budget().is_ok());
    }

    #[test]
    fn anthropic_price_table_charges_cache_reads_and_writes() {
        let meter = UsageMeter::with_prices(PriceTable::from(&anthropic()), 100.0);
        meter.record(TokenUsage {
            input_tokens: 1_000_000,
            cached_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            output_tokens: 1_000_000,
        });
        // 5 + 0.5 + 6.25 + 25
        assert!((meter.cost_usd() - 36.75).abs() < 1e-9);
    }

    #[test]
    fn meter_trips_the_budget_flag_and_stays_tripped() {
        // Ceiling of $0.10; 1M cache-miss input tokens costs $0.14.
        let meter = deepseek_meter(0.10);
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
        let meter = deepseek_meter(1.0);
        meter.preload_cost(0.5);
        assert!(!meter.budget_exceeded());
        assert!((meter.spent_usd() - 0.5).abs() < 1e-9);
        meter.preload_cost(1.5);
        assert!(meter.budget_exceeded());
    }

    #[test]
    fn openai_usage_split_uses_prompt_token_details() {
        let u: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 1000, "completion_tokens": 120, "total_tokens": 1120,
                "prompt_tokens_details": {"cached_tokens": 800}}"#,
        )
        .expect("fixture usage");
        assert_eq!(openai_usage(u), tokens(200, 800, 120));
    }

    #[test]
    fn openai_usage_falls_back_to_native_cache_fields() {
        let u: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 500, "completion_tokens": 40,
                "prompt_cache_hit_tokens": 448, "prompt_cache_miss_tokens": 52}"#,
        )
        .expect("fixture usage");
        assert_eq!(openai_usage(u), tokens(52, 448, 40));
        // Missing usage is not an error, just zero.
        let empty: OpenAiUsage = serde_json::from_str("{}").expect("empty usage");
        assert_eq!(openai_usage(empty), TokenUsage::default());
    }

    #[test]
    fn anthropic_usage_maps_cache_fields() {
        let u: AnthropicUsage = serde_json::from_str(
            r#"{"input_tokens": 120, "cache_creation_input_tokens": 3000,
                "cache_read_input_tokens": 0, "output_tokens": 800}"#,
        )
        .expect("fixture usage");
        assert_eq!(
            anthropic_usage(u),
            TokenUsage {
                input_tokens: 120,
                cached_tokens: 0,
                cache_write_tokens: 3000,
                output_tokens: 800,
            }
        );
    }

    #[test]
    fn code_fences_are_stripped() {
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    // -----------------------------------------------------------------------
    // LlmClient over the mock backend
    // -----------------------------------------------------------------------

    fn client(backend: Arc<MockBackend>, limit: f64) -> LlmClient {
        LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM PROMPT".into(),
            deepseek_meter(limit),
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

    #[tokio::test]
    async fn refusals_are_not_retried_and_keep_their_variant() {
        let backend = Arc::new(MockBackend::new());
        backend.push_llm_error(LlmError::refusal("anthropic"));
        backend.push("{}", TokenUsage::default());
        let llm = client(Arc::clone(&backend), 2.0).with_retry(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        });
        let err = llm.complete_text("x", 0.3).await.expect_err("refusal");
        assert!(matches!(err, LlmError::Refusal { ref provider } if provider == "anthropic"));
        assert_eq!(backend.calls(), 1, "a refusal is terminal for that client");
    }

    #[test]
    fn missing_api_keys_name_their_provider() {
        let blank = ProviderConfig {
            api_key: Some("   ".into()),
            ..deepseek()
        };
        let err = OpenAiCompatibleBackend::new("bulkprov", &blank).expect_err("blank key");
        assert!(
            matches!(err, LlmError::MissingApiKey { ref provider, .. } if provider == "bulkprov")
        );
        assert!(
            err.to_string()
                .contains("DAILY_EPUB_PROVIDERS__BULKPROV__API_KEY"),
            "{err}"
        );

        let err = AnthropicBackend::new("anthropic", &anthropic()).expect_err("no key");
        assert!(
            matches!(err, LlmError::MissingApiKey { ref provider, .. } if provider == "anthropic")
        );
        assert!(
            err.to_string()
                .contains("DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY")
        );

        // The dispatching constructor reports the same error for either kind.
        let err = LlmClient::for_provider(
            "gemini",
            &ProviderConfig::gemini(),
            "S".into(),
            deepseek_meter(1.0),
        )
        .expect_err("no key");
        assert!(
            err.to_string()
                .contains("DAILY_EPUB_PROVIDERS__GEMINI__API_KEY")
        );
    }

    // -----------------------------------------------------------------------
    // Llms
    // -----------------------------------------------------------------------

    fn mock_client(provider: &'static str, limit: f64) -> (LlmClient, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::new());
        let prices = if provider == "anthropic" {
            PriceTable::from(&anthropic())
        } else {
            PriceTable::from(&deepseek())
        };
        let client = LlmClient::with_backend_options(
            provider,
            "model",
            "SYSTEM".into(),
            None,
            UsageMeter::with_prices(prices, limit),
            Arc::clone(&backend) as Arc<dyn ChatBackend>,
        );
        (client, backend)
    }

    #[test]
    fn editor_or_bulk_prefers_an_untripped_editor() {
        let (bulk, _) = mock_client("deepseek", 2.0);
        let (editor, _) = mock_client("anthropic", 3.0);
        let llms = Llms {
            bulk: Some(bulk),
            editor: Some(editor),
        };
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("anthropic")
        );
        // Trip the editor's meter: bulk takes over.
        llms.editor
            .as_ref()
            .expect("editor")
            .meter
            .preload_cost(10.0);
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("deepseek")
        );
        // No bulk and a tripped editor means no client at all.
        let only_editor = Llms {
            bulk: None,
            editor: llms.editor.clone(),
        };
        assert!(only_editor.editor_or_bulk().is_none());
        assert!(Llms::default().is_empty());
        assert!(Llms::default().editor_or_bulk().is_none());
    }

    #[test]
    fn from_config_without_keys_yields_no_clients() {
        let config = Config::default();
        let meters = provider_meters(&config);
        assert_eq!(
            meters.keys().collect::<Vec<_>>(),
            vec!["anthropic", "deepseek"],
            "one meter per referenced provider, not per registry entry"
        );
        let llms = Llms::from_config(&config, "SYSTEM".into(), &meters);
        assert!(llms.is_empty());
    }

    fn keyed_config() -> Config {
        let mut config = Config::default();
        for (name, provider) in config.providers.iter_mut() {
            provider.api_key = Some(format!("{name}-key"));
        }
        config
    }

    #[test]
    fn from_config_resolves_both_roles_by_name() {
        let mut config = keyed_config();
        config.llm.editor = "gemini".into();
        config
            .providers
            .get_mut("deepseek")
            .unwrap()
            .max_concurrent_requests = 7;
        let meters = provider_meters(&config);
        let llms = Llms::from_config(&config, "SYSTEM".into(), &meters);

        let bulk = llms.bulk.as_ref().expect("bulk");
        assert_eq!(bulk.provider(), "deepseek");
        assert_eq!(bulk.model, "deepseek-v4-flash");
        assert_eq!(bulk.effort, None);
        assert_eq!(bulk.max_concurrent_requests, 7);
        assert_eq!(bulk.meter.limit_usd(), 2.0);
        let editor = llms.editor.as_ref().expect("editor");
        assert_eq!(editor.provider(), "gemini");
        assert_eq!(editor.model, "gemini-3.8-flash");
        assert_eq!(editor.effort.as_deref(), Some("high"));
        assert_eq!(editor.meter.limit_usd(), 3.0);
        assert_eq!(bulk.system_prompt, editor.system_prompt);
        // The clients share the meters the pipeline preloads and reports from.
        meters["gemini"].preload_cost(0.25);
        assert!((editor.meter.spent_usd() - 0.25).abs() < 1e-9);
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("gemini")
        );

        // The Anthropic kind resolves the same way.
        let config = keyed_config();
        let llms = Llms::from_config(&config, "SYSTEM".into(), &provider_meters(&config));
        let editor = llms.editor.as_ref().expect("editor");
        assert_eq!(editor.provider(), "anthropic");
        assert_eq!(editor.model, "claude-opus-5");
    }

    #[test]
    fn same_provider_for_both_roles_shares_one_client_and_meter() {
        let mut config = keyed_config();
        config.llm.bulk = "gemini".into();
        config.llm.editor = "gemini".into();
        let meters = provider_meters(&config);
        assert_eq!(meters.len(), 1);
        let llms = Llms::from_config(&config, "SYSTEM".into(), &meters);
        let bulk = llms.bulk.as_ref().expect("bulk");
        let editor = llms.editor.as_ref().expect("editor");
        assert!(Arc::ptr_eq(&bulk.backend, &editor.backend));
        assert!(Arc::ptr_eq(&bulk.meter.inner, &editor.meter.inner));
        assert_eq!(bulk.provider(), editor.provider());
    }

    #[test]
    fn missing_key_leaves_that_role_empty_and_no_editor_is_honoured() {
        let mut config = keyed_config();
        config.providers.get_mut("anthropic").unwrap().api_key = None;
        let llms = Llms::from_config(&config, "SYSTEM".into(), &provider_meters(&config));
        assert!(llms.bulk.is_some());
        assert!(llms.editor.is_none(), "no key, no editor client");
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("deepseek")
        );

        let mut config = keyed_config();
        config.llm.editor.clear();
        let llms = Llms::from_config(&config, "SYSTEM".into(), &provider_meters(&config));
        assert!(llms.editor.is_none());
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("deepseek")
        );

        let mut config = keyed_config();
        config.llm.bulk.clear();
        let llms = Llms::from_config(&config, "SYSTEM".into(), &provider_meters(&config));
        assert!(llms.bulk.is_none());
        assert_eq!(
            llms.editor_or_bulk().map(LlmClient::provider),
            Some("anthropic")
        );
    }

    // -----------------------------------------------------------------------
    // AnthropicBackend against a loopback listener (§4.2, §20)
    // -----------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct FakeServer {
        seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
        scripted: Arc<Mutex<VecDeque<(StatusCode, serde_json::Value)>>>,
    }

    impl FakeServer {
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
        State(fake): State<FakeServer>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        fake.seen.lock().expect("seen lock").push((headers, body));
        let (status, body) = fake
            .scripted
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "unscripted"}),
            ));
        (status, Json(body))
    }

    async fn serve(fake: FakeServer) -> String {
        let app = Router::new()
            .route("/v1/messages", post(handle))
            .route("/chat/completions", post(handle))
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

    async fn anthropic_client(fake: FakeServer, limit: f64) -> LlmClient {
        let base_url = serve(fake).await;
        let config = ProviderConfig {
            base_url,
            api_key: Some("test-key-never-logged".into()),
            effort: Some("medium".into()),
            ..anthropic()
        };
        LlmClient::for_provider(
            "anthropic",
            &config,
            "PROFILE SYSTEM PROMPT".into(),
            UsageMeter::with_prices(PriceTable::from(&config), limit),
        )
        .expect("client")
        .with_retry(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        })
    }

    fn ok_message(text: &str, stop_reason: &str) -> serde_json::Value {
        json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": text}
            ],
            "stop_reason": stop_reason,
            "usage": {
                "input_tokens": 1_000_000,
                "cache_creation_input_tokens": 1_000_000,
                "cache_read_input_tokens": 1_000_000,
                "output_tokens": 1_000_000
            }
        })
    }

    #[tokio::test]
    async fn anthropic_request_has_the_documented_shape() {
        let fake = FakeServer::default();
        fake.push(
            StatusCode::OK,
            ok_message("```json\n{\"ok\": true}\n```", "end_turn"),
        );
        let llm = anthropic_client(fake.clone(), 100.0).await;

        let out: serde_json::Value = llm
            .complete_json("the task", 0.7)
            .await
            .expect("completion");
        assert_eq!(out, json!({"ok": true}));

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("test-key-never-logged")
        );
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        assert_eq!(
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("server-side-fallback-2026-07-01")
        );
        assert!(
            headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json"))
        );

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["max_tokens"], 16_000);
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "PROFILE SYSTEM PROMPT");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "the task");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["fallbacks"], "default");
        for forbidden in [
            "temperature",
            "top_p",
            "top_k",
            "thinking",
            "response_format",
        ] {
            assert!(
                body.get(forbidden).is_none(),
                "{forbidden} must not be sent"
            );
        }
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(1),
            "no prefill"
        );

        // Usage was priced with the cache read/write rates: 5 + 6.25 + 0.5 + 25.
        assert_eq!(
            llm.meter.total(),
            TokenUsage {
                input_tokens: 1_000_000,
                cached_tokens: 1_000_000,
                cache_write_tokens: 1_000_000,
                output_tokens: 1_000_000,
            }
        );
        assert!((llm.meter.cost_usd() - 36.75).abs() < 1e-9);
    }

    #[tokio::test]
    async fn anthropic_refusal_surfaces_as_the_fallback_error() {
        let fake = FakeServer::default();
        fake.push(
            StatusCode::OK,
            json!({
                "content": [],
                "stop_reason": "refusal",
                "stop_details": {"type": "refusal", "category": "cyber"},
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }),
        );
        let llm = anthropic_client(fake.clone(), 100.0).await;
        let err = llm.complete_text("x", 0.3).await.expect_err("refusal");
        assert!(matches!(err, LlmError::Refusal { ref provider } if provider == "anthropic"));
        assert!(!err.is_transient());
        assert_eq!(fake.requests().len(), 1, "a refusal is never retried");
    }

    #[tokio::test]
    async fn anthropic_429_is_retried_but_400_is_not() {
        let fake = FakeServer::default();
        fake.push(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type": "error", "error": {"type": "rate_limit_error"}}),
        );
        fake.push(
            StatusCode::OK,
            ok_message("{\"after\": \"retry\"}", "end_turn"),
        );
        let llm = anthropic_client(fake.clone(), 100.0).await;
        let text = llm.complete_text("x", 0.3).await.expect("second attempt");
        assert_eq!(text, "{\"after\": \"retry\"}");
        assert_eq!(fake.requests().len(), 2);

        let fake = FakeServer::default();
        fake.push(
            StatusCode::BAD_REQUEST,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "nope"}}),
        );
        let llm = anthropic_client(fake.clone(), 100.0).await;
        let err = llm.complete_text("x", 0.3).await.expect_err("400");
        assert!(matches!(err, LlmError::Api { ref provider, .. } if provider == "anthropic"));
        assert!(err.to_string().contains("400"));
        assert_eq!(fake.requests().len(), 1, "400 is never retried");
    }

    #[tokio::test]
    async fn anthropic_concatenates_text_blocks_and_rejects_empty_output() {
        let fake = FakeServer::default();
        fake.push(
            StatusCode::OK,
            json!({
                "content": [
                    {"type": "text", "text": "{\"a\": "},
                    {"type": "thinking", "thinking": "..."},
                    {"type": "text", "text": "1}"}
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
        );
        fake.push(
            StatusCode::OK,
            json!({
                "content": [{"type": "thinking", "thinking": ""}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }),
        );
        let llm = anthropic_client(fake.clone(), 100.0).await;
        let out: serde_json::Value = llm.complete_json("x", 0.3).await.expect("joined");
        assert_eq!(out, json!({"a": 1}));
        let err = llm.complete_text("y", 0.3).await.expect_err("empty");
        assert!(matches!(err, LlmError::EmptyResponse { ref provider } if provider == "anthropic"));
    }

    // -----------------------------------------------------------------------
    // OpenAiCompatibleBackend against a loopback listener
    // -----------------------------------------------------------------------

    async fn openai_client(fake: FakeServer, name: &str, effort: Option<&str>) -> LlmClient {
        let base_url = serve(fake).await;
        let config = ProviderConfig {
            base_url,
            api_key: Some("bearer-key-never-logged".into()),
            effort: effort.map(str::to_string),
            ..ProviderConfig::gemini()
        };
        LlmClient::for_provider(
            name,
            &config,
            "PROFILE SYSTEM PROMPT".into(),
            UsageMeter::for_provider(&config),
        )
        .expect("client")
        .with_retry(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        })
    }

    fn ok_completion(text: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-01",
            "object": "chat.completion",
            "model": "gemini-3.8-flash",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 300,
                "total_tokens": 1300,
                "prompt_tokens_details": {"cached_tokens": 800},
                "completion_tokens_details": {"reasoning_tokens": 250}
            }
        })
    }

    #[tokio::test]
    async fn openai_request_carries_effort_json_mode_and_bearer_key() {
        let fake = FakeServer::default();
        fake.push(StatusCode::OK, ok_completion("{\"ok\": true}"));
        let llm = openai_client(fake.clone(), "gemini", Some("high")).await;
        let out: serde_json::Value = llm
            .complete_json("the task", 0.3)
            .await
            .expect("completion");
        assert_eq!(out, json!({"ok": true}));

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer bearer-key-never-logged")
        );
        assert_eq!(body["model"], "gemini-3.8-flash");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "PROFILE SYSTEM PROMPT");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "the task");
        let temperature = body["temperature"].as_f64().expect("temperature");
        assert!(
            (temperature - 0.3).abs() < 1e-6,
            "f32 widened: {temperature}"
        );
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("output_config").is_none());

        // Cached prompt tokens come from `prompt_tokens_details`; reasoning
        // tokens are already inside `completion_tokens` and are not added twice.
        assert_eq!(llm.meter.total(), tokens(200, 800, 300));
        let expected = 200.0 * 0.75 / 1e6 + 800.0 * 0.075 / 1e6 + 300.0 * 3.75 / 1e6;
        assert!((llm.meter.cost_usd() - expected).abs() < 1e-12);
    }

    #[tokio::test]
    async fn openai_request_omits_effort_and_json_mode_when_unset() {
        let fake = FakeServer::default();
        fake.push(StatusCode::OK, ok_completion("plain prose"));
        let llm = openai_client(fake.clone(), "deepseek", None).await;
        let text = llm.complete_text("write", 0.8).await.expect("completion");
        assert_eq!(text, "plain prose");
        let (_, body) = &fake.requests()[0];
        assert!(
            body.get("reasoning_effort").is_none(),
            "no effort configured"
        );
        assert!(body.get("response_format").is_none(), "not a json call");
        assert_eq!(body["stream"], false);
    }

    #[tokio::test]
    async fn openai_errors_name_the_provider_and_retry_only_transient_statuses() {
        let fake = FakeServer::default();
        fake.push(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "warming up"}),
        );
        fake.push(StatusCode::OK, ok_completion("after retry"));
        let llm = openai_client(fake.clone(), "bulkprov", None).await;
        assert_eq!(
            llm.complete_text("x", 0.3).await.expect("retried"),
            "after retry"
        );
        assert_eq!(fake.requests().len(), 2);

        let fake = FakeServer::default();
        fake.push(
            StatusCode::UNAUTHORIZED,
            json!({"error": {"message": "bad key"}}),
        );
        let llm = openai_client(fake.clone(), "bulkprov", None).await;
        let err = llm.complete_text("x", 0.3).await.expect_err("401");
        assert!(matches!(err, LlmError::Api { ref provider, .. } if provider == "bulkprov"));
        assert!(
            err.to_string().starts_with("bulkprov request failed"),
            "{err}"
        );
        assert_eq!(fake.requests().len(), 1, "401 is never retried");

        let fake = FakeServer::default();
        fake.push(
            StatusCode::OK,
            json!({"choices": [{"message": {"role": "assistant", "content": ""}}]}),
        );
        let llm = openai_client(fake.clone(), "bulkprov", None).await;
        let err = llm.complete_text("x", 0.3).await.expect_err("empty");
        assert!(matches!(err, LlmError::EmptyResponse { ref provider } if provider == "bulkprov"));
    }
}
