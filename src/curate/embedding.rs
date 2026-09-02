//! Voyage embeddings, the f32 BLOB codec, and the SQLite cache (plan §4.3,
//! §7.1–7.2, §16 `features backfill`).
//!
//! The network is reached through an [`EmbeddingBackend`] so tests can inject
//! canned vectors ([`MockBackend`]) without touching the wire, mirroring
//! `ChatBackend` in `llm.rs`. Nothing here is fatal to a run: a failed batch
//! leaves its articles without embeddings and the caller carries on (§17).
//! Raw vectors never reach logs or reports.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::{StreamExt as _, stream};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;

use crate::config::{Config, VoyageConfig};
use crate::curate::{approx_tokens, profile, prompt_text};
use crate::db::{Db, fmt_ts};
use crate::http::RetryPolicy;
use crate::types::{Article, ArticleId};

/// The only place the Voyage key comes from (§4.3).
pub const VOYAGE_API_KEY_ENV: &str = "DAILY_EPUB_VOYAGE__API_KEY";
/// USD per million tokens, `voyage-4-lite` (§4.3, verified 2026-08-17).
pub const VOYAGE_PRICE_PER_MTOK: f64 = 0.02;
/// `features backfill` asks before spending more than this without `--yes` (§16).
pub const BACKFILL_CONFIRM_TOKENS: i64 = 5_000_000;
const EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("voyage api key is not configured (set DAILY_EPUB_VOYAGE__API_KEY)")]
    MissingApiKey,
    #[error("voyage request failed: {0}")]
    Api(String),
    /// A 5xx/429/network failure: worth retrying.
    #[error("voyage request failed (transient): {0}")]
    Transient(String),
    #[error("voyage response index {index} is invalid for {len} inputs")]
    InvalidIndex { index: usize, len: usize },
    #[error("voyage response contained duplicate index {0}")]
    DuplicateIndex(usize),
    #[error("voyage response returned {actual} vectors for {expected} inputs")]
    ResponseLength { expected: usize, actual: usize },
    #[error("embedding dimension mismatch: expected {expected}, got {actual}")]
    Dimension { expected: usize, actual: usize },
    #[error("embedding contains a non-finite value")]
    NonFinite,
    #[error("embedding blob length {actual} does not match dimension {dimension}")]
    BlobLength { dimension: usize, actual: usize },
    #[error("voyage daily cost ceiling of ${limit:.2} reached")]
    BudgetExceeded { limit: f64 },
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl EmbeddingError {
    fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Voyage's `input_type`: documents for articles, queries for interests (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Document,
    Query,
}

#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub input: Vec<String>,
    pub model: String,
    pub input_type: InputType,
    pub output_dimension: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexedEmbedding {
    pub index: usize,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmbeddingCompletion {
    /// Ordered by `index`, one per input.
    pub data: Vec<IndexedEmbedding>,
    pub total_tokens: i64,
}

/// Network seam matching `ChatBackend`; tests inject canned completions.
pub trait EmbeddingBackend: std::fmt::Debug + Send + Sync {
    fn embed<'a>(
        &'a self,
        request: EmbeddingRequest,
    ) -> BoxFuture<'a, Result<EmbeddingCompletion, EmbeddingError>>;
}

/// `POST {base_url}/embeddings` with the bearer key from
/// `DAILY_EPUB_VOYAGE__API_KEY` (§4.3). The key is never logged.
#[derive(Debug, Clone)]
pub struct VoyageBackend {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl VoyageBackend {
    pub fn new(config: &VoyageConfig) -> Result<Self, EmbeddingError> {
        // The config field is how figment carries the env var; the direct
        // read covers callers that built the config by hand.
        let api_key = config
            .api_key
            .clone()
            .or_else(|| std::env::var(VOYAGE_API_KEY_ENV).ok())
            .filter(|value| !value.trim().is_empty())
            .ok_or(EmbeddingError::MissingApiKey)?;
        let http = crate::http::build_client(EMBEDDING_TIMEOUT)
            .map_err(|error| EmbeddingError::Api(format!("building HTTP client: {error}")))?;
        Ok(Self {
            http,
            endpoint: format!("{}/embeddings", config.base_url.trim_end_matches('/')),
            api_key,
        })
    }
}

#[derive(Debug, Serialize)]
struct ApiRequest<'a> {
    input: &'a [String],
    model: &'a str,
    input_type: InputType,
    truncation: bool,
    output_dimension: usize,
    output_dtype: &'static str,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    data: Vec<ApiEmbedding>,
    #[serde(default)]
    usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
struct ApiEmbedding {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    total_tokens: i64,
}

impl EmbeddingBackend for VoyageBackend {
    fn embed<'a>(
        &'a self,
        request: EmbeddingRequest,
    ) -> BoxFuture<'a, Result<EmbeddingCompletion, EmbeddingError>> {
        Box::pin(async move {
            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&ApiRequest {
                    input: &request.input,
                    model: &request.model,
                    input_type: request.input_type,
                    truncation: true,
                    output_dimension: request.output_dimension,
                    output_dtype: "float",
                })
                .send()
                .await
                .map_err(|error| {
                    if crate::http::is_retryable(&error) {
                        EmbeddingError::Transient(error.to_string())
                    } else {
                        EmbeddingError::Api(error.to_string())
                    }
                })?;
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let message = format!("{status}: {}", detail.chars().take(500).collect::<String>());
                return Err(if status.is_server_error() || status.as_u16() == 429 {
                    EmbeddingError::Transient(message)
                } else {
                    EmbeddingError::Api(message)
                });
            }
            let parsed: ApiResponse = response
                .json()
                .await
                .map_err(|error| EmbeddingError::Api(format!("decoding response: {error}")))?;
            map_response(parsed, request.input.len(), request.output_dimension)
        })
    }
}

/// Order the response by `index` and reject short, long, duplicate or
/// malformed vectors (§4.3).
fn map_response(
    response: ApiResponse,
    expected: usize,
    dimension: usize,
) -> Result<EmbeddingCompletion, EmbeddingError> {
    let actual = response.data.len();
    if actual != expected {
        return Err(EmbeddingError::ResponseLength { expected, actual });
    }
    let mut ordered: Vec<Option<IndexedEmbedding>> = vec![None; expected];
    for item in response.data {
        if item.index >= expected {
            return Err(EmbeddingError::InvalidIndex {
                index: item.index,
                len: expected,
            });
        }
        validate_vector(&item.embedding, dimension)?;
        let index = item.index;
        if ordered[index]
            .replace(IndexedEmbedding {
                index,
                embedding: item.embedding,
            })
            .is_some()
        {
            return Err(EmbeddingError::DuplicateIndex(index));
        }
    }
    Ok(EmbeddingCompletion {
        data: ordered.into_iter().flatten().collect(),
        total_tokens: response.usage.total_tokens.max(0),
    })
}

/// Voyage token meter with the `max_daily_usd` runaway guard (§5).
#[derive(Debug, Clone)]
pub struct UsageMeter {
    tokens: Arc<Mutex<i64>>,
    max_daily_usd: f64,
}

impl UsageMeter {
    pub fn new(max_daily_usd: f64) -> Self {
        Self {
            tokens: Arc::new(Mutex::new(0)),
            max_daily_usd,
        }
    }

    pub fn total_tokens(&self) -> i64 {
        match self.tokens.lock() {
            Ok(tokens) => *tokens,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub fn cost_usd(&self) -> f64 {
        cost_for_tokens(self.total_tokens())
    }

    fn check(&self) -> Result<(), EmbeddingError> {
        if self.max_daily_usd > 0.0 && self.cost_usd() >= self.max_daily_usd {
            Err(EmbeddingError::BudgetExceeded {
                limit: self.max_daily_usd,
            })
        } else {
            Ok(())
        }
    }

    fn record(&self, tokens: i64) {
        match self.tokens.lock() {
            Ok(mut total) => *total += tokens.max(0),
            Err(poisoned) => *poisoned.into_inner() += tokens.max(0),
        }
    }
}

pub fn cost_for_tokens(tokens: i64) -> f64 {
    tokens as f64 * VOYAGE_PRICE_PER_MTOK / 1_000_000.0
}

/// Batching, bounded concurrency, retries and metering over a backend (§4.3).
#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    config: VoyageConfig,
    backend: Arc<dyn EmbeddingBackend>,
    retry: RetryPolicy,
    pub meter: UsageMeter,
}

impl EmbeddingClient {
    pub fn new(config: &VoyageConfig) -> Result<Self, EmbeddingError> {
        Ok(Self::with_backend(
            config.clone(),
            Arc::new(VoyageBackend::new(config)?),
        ))
    }

    pub fn with_backend(config: VoyageConfig, backend: Arc<dyn EmbeddingBackend>) -> Self {
        Self {
            meter: UsageMeter::new(config.max_daily_usd),
            config,
            backend,
            retry: RetryPolicy::default(),
        }
    }

    /// Embed every text in `batch_size` chunks, at most `max_concurrent_requests`
    /// in flight. A failed batch yields `None` for its texts and is logged.
    pub async fn embed_many(
        &self,
        texts: &[String],
        input_type: InputType,
    ) -> Vec<Option<Vec<f32>>> {
        if texts.is_empty() {
            return Vec::new();
        }
        let batch_size = self.config.batch_size.max(1);
        let batches = texts
            .chunks(batch_size)
            .enumerate()
            .map(|(batch_index, chunk)| (batch_index * batch_size, chunk.to_vec()));
        let client = self.clone();
        let mut completed = stream::iter(batches.map(move |(offset, input)| {
            let client = client.clone();
            async move {
                let result = client.embed_batch(input, input_type).await;
                (offset, result)
            }
        }))
        .buffer_unordered(self.config.max_concurrent_requests.max(1));

        let mut output = vec![None; texts.len()];
        while let Some((offset, result)) = completed.next().await {
            match result {
                Ok(vectors) => {
                    for (index, vector) in vectors.into_iter().enumerate() {
                        if let Some(slot) = output.get_mut(offset + index) {
                            *slot = Some(vector);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, offset, "voyage batch failed; leaving its embeddings absent")
                }
            }
        }
        output
    }

    async fn embed_batch(
        &self,
        input: Vec<String>,
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.meter.check()?;
        let request = EmbeddingRequest {
            input,
            model: self.config.model.clone(),
            input_type,
            output_dimension: self.config.output_dimension,
        };
        let completion = self
            .retry
            .run("voyage embeddings", EmbeddingError::is_transient, || {
                self.backend.embed(request.clone())
            })
            .await?;
        self.meter.record(completion.total_tokens);
        Ok(completion
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Codec and arithmetic (§7.1)
// ---------------------------------------------------------------------------

/// f32 little-endian, `dimension * 4` bytes; non-finite values are rejected.
pub fn encode_blob(vector: &[f32]) -> Result<Vec<u8>, EmbeddingError> {
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(EmbeddingError::NonFinite);
    }
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

pub fn decode_blob(bytes: &[u8], dimension: usize) -> Result<Vec<f32>, EmbeddingError> {
    if bytes.len() != dimension.saturating_mul(4) {
        return Err(EmbeddingError::BlobLength {
            dimension,
            actual: bytes.len(),
        });
    }
    let mut vector = Vec::with_capacity(dimension);
    for chunk in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(EmbeddingError::NonFinite);
        }
        vector.push(value);
    }
    Ok(vector)
}

/// Dot product (= cosine, Voyage vectors are unit-normalized) with a dimension check.
pub fn dot(left: &[f32], right: &[f32]) -> Result<f64, EmbeddingError> {
    if left.len() != right.len() {
        return Err(EmbeddingError::Dimension {
            expected: left.len(),
            actual: right.len(),
        });
    }
    Ok(left
        .iter()
        .zip(right)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum())
}

fn validate_vector(vector: &[f32], dimension: usize) -> Result<(), EmbeddingError> {
    if vector.len() != dimension {
        return Err(EmbeddingError::Dimension {
            expected: dimension,
            actual: vector.len(),
        });
    }
    encode_blob(vector).map(|_| ())
}

// ---------------------------------------------------------------------------
// Embedded text (§7.1)
// ---------------------------------------------------------------------------

/// `"Title: {title}\n\n{plain body}"`, whitespace collapsed, cut at
/// `max_chars` on a char boundary. Deliberately no feed name, author or scores.
pub fn article_input(article: &Article, max_chars: usize) -> String {
    let body = prompt_text(&article.content_html);
    let title = article
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&format!("Title: {title}\n\n{body}"), max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte, _)) => text[..byte].to_string(),
        None => text.to_string(),
    }
}

/// `sha256` of the embedded text, the cache key alongside model and dimension.
pub fn input_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

// ---------------------------------------------------------------------------
// Cache orchestration (§7.1, §7.2)
// ---------------------------------------------------------------------------

/// A cache miss waiting for the network.
#[derive(Debug, Clone)]
struct ArticleMiss {
    article_id: ArticleId,
    text: String,
    hash: String,
}

/// The `article_embeddings` / `interest_embeddings` cache in front of a client.
///
/// Without a client (`--skip-embeddings`, Voyage disabled, no key) it answers
/// from the cache only and never touches the network.
#[derive(Debug, Clone)]
pub struct EmbeddingService {
    db: Db,
    config: VoyageConfig,
    client: Option<EmbeddingClient>,
}

impl EmbeddingService {
    pub fn cached_only(db: Db, config: VoyageConfig) -> Self {
        Self {
            db,
            config,
            client: None,
        }
    }

    pub fn real(db: Db, config: VoyageConfig) -> Result<Self, EmbeddingError> {
        let client = EmbeddingClient::new(&config)?;
        Ok(Self::with_client(db, config, client))
    }

    pub fn with_client(db: Db, config: VoyageConfig, client: EmbeddingClient) -> Self {
        Self {
            db,
            config,
            client: Some(client),
        }
    }

    pub fn config(&self) -> &VoyageConfig {
        &self.config
    }

    /// `None` when the service is cache-only.
    pub fn meter(&self) -> Option<&UsageMeter> {
        self.client.as_ref().map(|client| &client.meter)
    }

    pub fn has_client(&self) -> bool {
        self.client.is_some()
    }

    /// Split the articles into cached vectors and misses (no network).
    async fn lookup_articles(
        &self,
        articles: &[Article],
    ) -> Result<(HashMap<ArticleId, Vec<f32>>, Vec<ArticleMiss>), EmbeddingError> {
        let mut found = HashMap::new();
        let mut misses = Vec::new();
        for article in articles {
            let text = article_input(article, self.config.max_input_chars);
            let hash = input_hash(&text);
            let row = sqlx::query(
                "SELECT embedding FROM article_embeddings
                 WHERE article_id = ? AND model = ? AND dimension = ? AND input_hash = ?",
            )
            .bind(article.id)
            .bind(&self.config.model)
            .bind(self.config.output_dimension as i64)
            .bind(&hash)
            .fetch_optional(self.db.pool())
            .await?;
            let cached = row.and_then(|row| {
                decode_blob(
                    &row.get::<Vec<u8>, _>("embedding"),
                    self.config.output_dimension,
                )
                .map_err(|error| {
                    tracing::warn!(article_id = article.id, %error, "ignoring a malformed cached embedding")
                })
                .ok()
            });
            match cached {
                Some(vector) => {
                    found.insert(article.id, vector);
                }
                None => misses.push(ArticleMiss {
                    article_id: article.id,
                    text,
                    hash,
                }),
            }
        }
        Ok((found, misses))
    }

    /// Articles with no usable cached vector, with their estimated token cost.
    pub async fn uncached_articles(
        &self,
        articles: &[Article],
    ) -> Result<Vec<(ArticleId, i64)>, EmbeddingError> {
        let (_, misses) = self.lookup_articles(articles).await?;
        Ok(misses
            .into_iter()
            .map(|miss| (miss.article_id, approx_tokens(&miss.text) as i64))
            .collect())
    }

    /// Cached-or-fetched vectors for every article that has one (§7.1).
    pub async fn articles(
        &self,
        articles: &[Article],
    ) -> Result<HashMap<ArticleId, Vec<f32>>, EmbeddingError> {
        let (mut found, misses) = self.lookup_articles(articles).await?;
        let Some(client) = self.client.as_ref() else {
            return Ok(found);
        };
        if misses.is_empty() {
            return Ok(found);
        }
        let texts = misses
            .iter()
            .map(|miss| miss.text.clone())
            .collect::<Vec<_>>();
        let vectors = client.embed_many(&texts, InputType::Document).await;
        let now = fmt_ts(Timestamp::now());
        for (miss, vector) in misses.into_iter().zip(vectors) {
            let Some(vector) = vector else { continue };
            let blob = encode_blob(&vector)?;
            sqlx::query(
                "INSERT INTO article_embeddings
                     (article_id, model, dimension, input_hash, embedding, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(article_id) DO UPDATE SET
                     model = excluded.model, dimension = excluded.dimension,
                     input_hash = excluded.input_hash, embedding = excluded.embedding,
                     created_at = excluded.created_at",
            )
            .bind(miss.article_id)
            .bind(&self.config.model)
            .bind(self.config.output_dimension as i64)
            .bind(&miss.hash)
            .bind(blob)
            .bind(&now)
            .execute(self.db.pool())
            .await?;
            found.insert(miss.article_id, vector);
        }
        Ok(found)
    }

    async fn lookup_interests(
        &self,
        interests: &[String],
    ) -> Result<(HashMap<String, Vec<f32>>, Vec<String>), EmbeddingError> {
        let mut found = HashMap::new();
        let mut misses = Vec::new();
        for interest in interests {
            let row = sqlx::query(
                "SELECT embedding FROM interest_embeddings
                 WHERE interest = ? AND model = ? AND dimension = ?",
            )
            .bind(interest)
            .bind(&self.config.model)
            .bind(self.config.output_dimension as i64)
            .fetch_optional(self.db.pool())
            .await?;
            let cached = row.and_then(|row| {
                decode_blob(
                    &row.get::<Vec<u8>, _>("embedding"),
                    self.config.output_dimension,
                )
                .map_err(|error| {
                    tracing::warn!(interest, %error, "ignoring a malformed cached interest embedding")
                })
                .ok()
            });
            match cached {
                Some(vector) => {
                    found.insert(interest.clone(), vector);
                }
                None => misses.push(interest.clone()),
            }
        }
        Ok((found, misses))
    }

    pub async fn uncached_interests(
        &self,
        interests: &[String],
    ) -> Result<Vec<String>, EmbeddingError> {
        Ok(self.lookup_interests(interests).await?.1)
    }

    /// Cached-or-fetched query vectors for the bare interest strings (§7.2).
    pub async fn interests(
        &self,
        interests: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, EmbeddingError> {
        let (mut found, misses) = self.lookup_interests(interests).await?;
        let Some(client) = self.client.as_ref() else {
            return Ok(found);
        };
        if misses.is_empty() {
            return Ok(found);
        }
        let vectors = client.embed_many(&misses, InputType::Query).await;
        let now = fmt_ts(Timestamp::now());
        for (interest, vector) in misses.into_iter().zip(vectors) {
            let Some(vector) = vector else { continue };
            let blob = encode_blob(&vector)?;
            sqlx::query(
                "INSERT INTO interest_embeddings
                     (interest, model, dimension, embedding, created_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(interest) DO UPDATE SET
                     model = excluded.model, dimension = excluded.dimension,
                     embedding = excluded.embedding, created_at = excluded.created_at",
            )
            .bind(&interest)
            .bind(&self.config.model)
            .bind(self.config.output_dimension as i64)
            .bind(blob)
            .bind(&now)
            .execute(self.db.pool())
            .await?;
            found.insert(interest, vector);
        }
        Ok(found)
    }
}

/// Cached vectors for the given ids under the configured model and dimension,
/// whatever text they were computed from (the rated set, §9.2).
pub async fn load_article_embeddings(
    db: &Db,
    config: &VoyageConfig,
    article_ids: &[ArticleId],
) -> Result<HashMap<ArticleId, Vec<f32>>, EmbeddingError> {
    let mut output = HashMap::new();
    for article_id in article_ids {
        let row = sqlx::query(
            "SELECT embedding FROM article_embeddings
             WHERE article_id = ? AND model = ? AND dimension = ?",
        )
        .bind(article_id)
        .bind(&config.model)
        .bind(config.output_dimension as i64)
        .fetch_optional(db.pool())
        .await?;
        if let Some(row) = row {
            match decode_blob(&row.get::<Vec<u8>, _>("embedding"), config.output_dimension) {
                Ok(vector) => {
                    output.insert(*article_id, vector);
                }
                Err(error) => tracing::warn!(article_id, %error, "ignoring a malformed embedding"),
            }
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// `features backfill` (§16)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct BackfillOptions {
    /// Window for published and (under `--all`) other articles, in days.
    pub days: i64,
    /// Only the rated set.
    pub rated_only: bool,
    /// Also every other article first seen inside the window.
    pub all: bool,
}

/// What a backfill would embed: cache misses only, in priority order.
#[derive(Debug, Default)]
pub struct BackfillPlan {
    /// Rated and published articles (the learned set), rated first.
    pub learned: Vec<Article>,
    pub interests: Vec<String>,
    /// Other recent articles; only under `--all`.
    pub others: Vec<Article>,
    pub estimated_tokens: i64,
    /// Articles and interests that were already cached and will be skipped.
    pub cached: usize,
}

impl BackfillPlan {
    pub fn is_empty(&self) -> bool {
        self.learned.is_empty() && self.interests.is_empty() && self.others.is_empty()
    }

    pub fn article_count(&self) -> usize {
        self.learned.len() + self.others.len()
    }

    pub fn estimated_cost_usd(&self) -> f64 {
        cost_for_tokens(self.estimated_tokens)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BackfillOutcome {
    pub articles_embedded: usize,
    pub interests_embedded: usize,
    pub tokens: i64,
    pub cost_usd: f64,
}

/// Decide what `features backfill` would embed without calling Voyage.
pub async fn plan_backfill(
    db: &Db,
    config: &Config,
    service: &EmbeddingService,
    opts: &BackfillOptions,
) -> anyhow::Result<BackfillPlan> {
    let since = Timestamp::now()
        .checked_sub(jiff::Span::new().hours(opts.days.max(0).saturating_mul(24)))
        .unwrap_or(Timestamp::UNIX_EPOCH);

    let mut ids = Vec::new();
    for rating in db
        .current_ratings(config.curation.ranking.rating_lookback_days)
        .await?
    {
        ids.push(rating.article_id);
    }
    if !opts.rated_only {
        ids.extend(db.published_article_ids_since(since).await?);
    }
    let mut seen = std::collections::HashSet::new();
    let mut learned = Vec::new();
    for id in ids {
        if seen.insert(id)
            && let Some(article) = db.get_article(id).await?
        {
            learned.push(article);
        }
    }

    let mut others = Vec::new();
    if opts.all && !opts.rated_only {
        for id in db.article_ids_since(since).await? {
            if seen.insert(id)
                && let Some(article) = db.get_article(id).await?
            {
                others.push(article);
            }
        }
    }

    let interests =
        match profile::load_standing_interests(&config.interests_opml, &config.profile_path) {
            Ok(interests) => interests,
            Err(error) => {
                tracing::warn!(%error, "could not load standing interests; skipping them");
                Vec::new()
            }
        };

    let mut plan = BackfillPlan::default();
    let mut keep = |articles: Vec<Article>, misses: Vec<(ArticleId, i64)>| -> Vec<Article> {
        let wanted: HashMap<ArticleId, i64> = misses.into_iter().collect();
        plan.cached += articles.len() - wanted.len();
        plan.estimated_tokens += wanted.values().sum::<i64>();
        articles
            .into_iter()
            .filter(|article| wanted.contains_key(&article.id))
            .collect()
    };
    let learned_misses = service.uncached_articles(&learned).await?;
    plan.learned = keep(learned, learned_misses);
    let other_misses = service.uncached_articles(&others).await?;
    plan.others = keep(others, other_misses);

    let interest_misses = service.uncached_interests(&interests).await?;
    plan.cached += interests.len() - interest_misses.len();
    plan.estimated_tokens += interest_misses
        .iter()
        .map(|interest| approx_tokens(interest) as i64)
        .sum::<i64>();
    plan.interests = interest_misses;
    Ok(plan)
}

/// Embed the plan in priority order: learned set, interests, then the rest.
pub async fn run_backfill(
    service: &EmbeddingService,
    plan: &BackfillPlan,
) -> anyhow::Result<BackfillOutcome> {
    let mut outcome = BackfillOutcome::default();
    outcome.articles_embedded += service.articles(&plan.learned).await?.len();
    outcome.interests_embedded += service.interests(&plan.interests).await?.len();
    outcome.articles_embedded += service.articles(&plan.others).await?.len();
    if let Some(meter) = service.meter() {
        outcome.tokens = meter.total_tokens();
        outcome.cost_usd = meter.cost_usd();
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Test backend
// ---------------------------------------------------------------------------

/// Canned-vector backend for tests: pops scripted replies in order.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockBackend {
    scripted: Mutex<std::collections::VecDeque<Result<EmbeddingCompletion, String>>>,
    /// Every request the code under test sent, in order.
    pub seen: Mutex<Vec<EmbeddingRequest>>,
    /// When set, every request is answered with this many-dimensional unit
    /// vectors derived from the input text (deterministic, no scripting).
    auto_dimension: Mutex<Option<usize>>,
}

#[cfg(test)]
impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer every request with deterministic vectors of this dimension.
    pub fn auto(dimension: usize) -> Self {
        Self {
            auto_dimension: Mutex::new(Some(dimension)),
            ..Self::default()
        }
    }

    pub fn push(&self, completion: EmbeddingCompletion) {
        self.scripted
            .lock()
            .expect("mock mutex")
            .push_back(Ok(completion));
    }

    pub fn push_error(&self, error: &str) {
        self.scripted
            .lock()
            .expect("mock mutex")
            .push_back(Err(error.to_string()));
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().expect("mock mutex").len()
    }

    pub fn requests(&self) -> Vec<EmbeddingRequest> {
        self.seen.lock().expect("mock mutex").clone()
    }

    /// A unit vector that depends only on the text, for cache tests.
    pub fn vector_for(text: &str, dimension: usize) -> Vec<f32> {
        let digest = Sha256::digest(text.as_bytes());
        let mut vector = (0..dimension)
            .map(|i| f32::from(digest[i % digest.len()]) / 255.0 - 0.5)
            .collect::<Vec<_>>();
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
        vector.iter_mut().for_each(|v| *v /= norm);
        vector
    }
}

#[cfg(test)]
impl EmbeddingBackend for MockBackend {
    fn embed<'a>(
        &'a self,
        request: EmbeddingRequest,
    ) -> BoxFuture<'a, Result<EmbeddingCompletion, EmbeddingError>> {
        Box::pin(async move {
            let auto = *self.auto_dimension.lock().expect("mock mutex");
            self.seen.lock().expect("mock mutex").push(request.clone());
            if let Some(dimension) = auto {
                return Ok(EmbeddingCompletion {
                    data: request
                        .input
                        .iter()
                        .enumerate()
                        .map(|(index, text)| IndexedEmbedding {
                            index,
                            embedding: Self::vector_for(text, dimension),
                        })
                        .collect(),
                    total_tokens: request
                        .input
                        .iter()
                        .map(|text| approx_tokens(text) as i64)
                        .sum(),
                });
            }
            match self.scripted.lock().expect("mock mutex").pop_front() {
                Some(Ok(completion)) => Ok(completion),
                Some(Err(error)) => Err(EmbeddingError::Api(error)),
                None => Err(EmbeddingError::Api("mock exhausted".into())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtractMethod, SourceKind, SourceRef};

    fn vector(index: usize, values: &[f32]) -> IndexedEmbedding {
        IndexedEmbedding {
            index,
            embedding: values.to_vec(),
        }
    }

    fn small_config() -> VoyageConfig {
        VoyageConfig {
            output_dimension: 4,
            batch_size: 2,
            max_concurrent_requests: 2,
            ..VoyageConfig::default()
        }
    }

    fn article(id: ArticleId, title: &str, body: &str) -> Article {
        Article {
            id,
            canonical_url: format!("https://example.com/{id}"),
            title: title.into(),
            best_entry_id: id,
            content_html: body.into(),
            word_count: 2,
            excerpt_only: false,
            image_count: 0,
            sources: vec![SourceRef {
                entry_id: id,
                feed_id: 9,
                feed_title: "Secret Feed".into(),
                category: None,
                kind: SourceKind::Feed,
            }],
            first_seen: "2026-08-15T00:00:00Z".parse().unwrap(),
            url: format!("https://example.com/{id}"),
            author: Some("Secret Author".into()),
            feed_id: 9,
            feed_title: "Secret Feed".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        }
    }

    async fn db_with_articles(ids: &[ArticleId]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("embed.db"))
            .await
            .unwrap();
        for id in ids {
            sqlx::query(
                "INSERT INTO articles (id, canonical_url, title, first_seen)
                 VALUES (?, ?, 'Article', '2026-08-15T00:00:00Z')",
            )
            .bind(id)
            .bind(format!("https://example.com/{id}"))
            .execute(db.pool())
            .await
            .unwrap();
        }
        (dir, db)
    }

    fn service(db: Db, config: VoyageConfig, backend: Arc<MockBackend>) -> EmbeddingService {
        let client = EmbeddingClient::with_backend(config.clone(), backend);
        EmbeddingService::with_client(db, config, client)
    }

    #[test]
    fn blob_round_trip_and_validation() {
        let values = vec![0.25, -1.5, 3.0];
        assert_eq!(
            decode_blob(&encode_blob(&values).unwrap(), 3).unwrap(),
            values
        );
        assert!(matches!(
            decode_blob(&[0; 4], 2),
            Err(EmbeddingError::BlobLength { .. })
        ));
        assert!(matches!(
            encode_blob(&[f32::NAN]),
            Err(EmbeddingError::NonFinite)
        ));
        let mut bytes = encode_blob(&[1.0, 2.0]).unwrap();
        bytes[4..].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert!(matches!(
            decode_blob(&bytes, 2),
            Err(EmbeddingError::NonFinite)
        ));
    }

    #[test]
    fn dot_checks_dimensions() {
        assert!((dot(&[1.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-9);
        assert!(matches!(
            dot(&[1.0], &[1.0, 0.0]),
            Err(EmbeddingError::Dimension { .. })
        ));
    }

    #[test]
    fn response_is_mapped_by_index_and_checked() {
        let response = ApiResponse {
            data: vec![
                ApiEmbedding {
                    index: 1,
                    embedding: vec![0.0, 1.0],
                },
                ApiEmbedding {
                    index: 0,
                    embedding: vec![1.0, 0.0],
                },
            ],
            usage: ApiUsage { total_tokens: 12 },
        };
        let mapped = map_response(response, 2, 2).unwrap();
        assert_eq!(mapped.data[0].embedding, [1.0, 0.0]);
        assert_eq!(mapped.data[1].embedding, [0.0, 1.0]);
        assert_eq!(mapped.total_tokens, 12);

        let short = ApiResponse {
            data: vec![ApiEmbedding {
                index: 0,
                embedding: vec![1.0],
            }],
            usage: ApiUsage::default(),
        };
        assert!(matches!(
            map_response(short, 2, 1),
            Err(EmbeddingError::ResponseLength { .. })
        ));
        let wrong_dimension = ApiResponse {
            data: vec![ApiEmbedding {
                index: 0,
                embedding: vec![1.0, 2.0, 3.0],
            }],
            usage: ApiUsage::default(),
        };
        assert!(matches!(
            map_response(wrong_dimension, 1, 2),
            Err(EmbeddingError::Dimension { .. })
        ));
        let duplicate = ApiResponse {
            data: vec![
                ApiEmbedding {
                    index: 0,
                    embedding: vec![1.0],
                },
                ApiEmbedding {
                    index: 0,
                    embedding: vec![1.0],
                },
            ],
            usage: ApiUsage::default(),
        };
        assert!(matches!(
            map_response(duplicate, 2, 1),
            Err(EmbeddingError::DuplicateIndex(0))
        ));
    }

    #[tokio::test]
    async fn one_failed_batch_does_not_abort_another() {
        let config = VoyageConfig {
            batch_size: 1,
            max_concurrent_requests: 1,
            output_dimension: 2,
            ..VoyageConfig::default()
        };
        let backend = Arc::new(MockBackend::new());
        backend.push_error("failed");
        backend.push(EmbeddingCompletion {
            data: vec![vector(0, &[1.0, 0.0])],
            total_tokens: 3,
        });
        let client = EmbeddingClient::with_backend(config, backend.clone());
        let result = client
            .embed_many(&["a".into(), "b".into()], InputType::Document)
            .await;
        assert!(result[0].is_none());
        assert_eq!(result[1].as_deref(), Some([1.0, 0.0].as_slice()));
        assert_eq!(backend.calls(), 2);
        assert_eq!(client.meter.total_tokens(), 3);
    }

    #[tokio::test]
    async fn the_budget_guard_stops_further_batches() {
        let config = VoyageConfig {
            batch_size: 1,
            max_concurrent_requests: 1,
            output_dimension: 1,
            max_daily_usd: 0.000_000_02, // one token
            ..VoyageConfig::default()
        };
        let backend = Arc::new(MockBackend::new());
        backend.push(EmbeddingCompletion {
            data: vec![vector(0, &[1.0])],
            total_tokens: 1,
        });
        backend.push(EmbeddingCompletion {
            data: vec![vector(0, &[1.0])],
            total_tokens: 1,
        });
        let client = EmbeddingClient::with_backend(config, backend.clone());
        let result = client
            .embed_many(&["a".into(), "b".into()], InputType::Document)
            .await;
        assert!(result[0].is_some());
        assert!(result[1].is_none());
        assert_eq!(backend.calls(), 1);
    }

    #[test]
    fn article_text_excludes_feed_author_and_collapses_markup() {
        let text = article_input(&article(1, "A  title", "<p>Hello   world</p>"), 60_000);
        assert_eq!(text, "Title: A title\n\nHello world");
        assert!(!text.contains("Secret Feed") && !text.contains("Secret Author"));
        // Cut on a char boundary.
        let cut = article_input(&article(1, "T", "héllo wörld"), 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.starts_with("Title: T\n\nh"));
    }

    #[tokio::test]
    async fn cache_hits_on_same_hash_and_misses_on_changed_text_model_or_dimension() {
        let (_dir, db) = db_with_articles(&[1]).await;
        let backend = Arc::new(MockBackend::auto(4));
        let config = small_config();
        let svc = service(db.clone(), config.clone(), backend.clone());
        let a = article(1, "Title", "<p>body</p>");

        let first = svc.articles(std::slice::from_ref(&a)).await.unwrap();
        assert_eq!(backend.calls(), 1);
        assert_eq!(first[&1].len(), 4);
        let request = &backend.requests()[0];
        assert_eq!(request.input_type, InputType::Document);
        assert_eq!(request.input[0], "Title: Title\n\nbody");
        assert_eq!(request.output_dimension, 4);

        // Same text → cache hit, no call.
        let again = svc.articles(std::slice::from_ref(&a)).await.unwrap();
        assert_eq!(backend.calls(), 1);
        assert_eq!(again[&1], first[&1]);
        assert!(
            svc.uncached_articles(std::slice::from_ref(&a))
                .await
                .unwrap()
                .is_empty()
        );

        // Changed text → new hash → miss, row overwritten.
        let edited = article(1, "Title", "<p>new body</p>");
        let after_edit = svc.articles(std::slice::from_ref(&edited)).await.unwrap();
        assert_eq!(backend.calls(), 2);
        assert_ne!(after_edit[&1], first[&1]);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM article_embeddings")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(rows, 1, "one row per article, overwritten");

        // Changed model → miss.
        let other_model = EmbeddingService::with_client(
            db.clone(),
            VoyageConfig {
                model: "voyage-other".into(),
                ..config.clone()
            },
            EmbeddingClient::with_backend(
                VoyageConfig {
                    model: "voyage-other".into(),
                    ..config.clone()
                },
                backend.clone(),
            ),
        );
        other_model
            .articles(std::slice::from_ref(&edited))
            .await
            .unwrap();
        assert_eq!(backend.calls(), 3);

        // Changed dimension → miss (the mock answers in the requested dimension).
        let backend8 = Arc::new(MockBackend::auto(8));
        let dim8 = VoyageConfig {
            output_dimension: 8,
            ..config.clone()
        };
        let other_dimension = service(db.clone(), dim8, backend8.clone());
        let vectors = other_dimension
            .articles(std::slice::from_ref(&edited))
            .await
            .unwrap();
        assert_eq!(backend8.calls(), 1);
        assert_eq!(vectors[&1].len(), 8);

        // Cache-only: no client, so a miss stays a miss and nothing is called.
        let cache_only = EmbeddingService::cached_only(db.clone(), small_config());
        let fresh = article(1, "Title", "<p>yet another body</p>");
        assert!(
            cache_only
                .articles(std::slice::from_ref(&fresh))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(cache_only.meter().is_none());
    }

    #[tokio::test]
    async fn interests_are_embedded_as_bare_queries_and_cached() {
        let (_dir, db) = db_with_articles(&[]).await;
        let backend = Arc::new(MockBackend::auto(4));
        let svc = service(db.clone(), small_config(), backend.clone());
        let interests = vec!["Writerdeck".to_string(), "Gaussian Splatting".to_string()];
        let first = svc.interests(&interests).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(backend.calls(), 1);
        let request = &backend.requests()[0];
        assert_eq!(request.input_type, InputType::Query);
        assert_eq!(request.input, interests);
        svc.interests(&interests).await.unwrap();
        assert_eq!(backend.calls(), 1, "warm cache makes no call");
        assert_eq!(
            load_article_embeddings(&db, &small_config(), &[1])
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn backfill_prioritizes_the_learned_set_and_is_idempotent() {
        let (dir, db) = db_with_articles(&[1, 2, 3]).await;
        // Article 1 is rated, article 2 is published, article 3 is neither.
        sqlx::query(
            "INSERT INTO rating_events (article_id, kind, source, label, value, event_at)
             VALUES (1, 'explicit', 'cli', 'loved', 1.0, ?)",
        )
        .bind(fmt_ts(Timestamp::now()))
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at) VALUES ('2026-08-15', 1, '2026-08-15T12:00:00Z');
             INSERT INTO issue_articles (issue_date, article_id, section) VALUES ('2026-08-15', 2, 'Top Stories');",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Article rows carry no body in this fixture; refresh them with one so
        // that `get_article` yields embeddable text.
        sqlx::query("UPDATE articles SET content_html = '<p>some body text</p>', first_seen = ?")
            .bind(fmt_ts(Timestamp::now()))
            .execute(db.pool())
            .await
            .unwrap();

        let config = Config {
            voyage: small_config(),
            interests_opml: dir.path().join("interests.opml"),
            profile_path: dir.path().join("profile.md"),
            ..Config::default()
        };
        std::fs::write(
            &config.interests_opml,
            "<opml><body><outline text=\"Writerdeck\"/></body></opml>",
        )
        .unwrap();
        std::fs::write(&config.profile_path, "# Reader profile\n").unwrap();

        let backend = Arc::new(MockBackend::auto(4));
        let svc = service(db.clone(), config.voyage.clone(), backend.clone());
        let opts = BackfillOptions {
            days: 30,
            rated_only: false,
            all: false,
        };
        let plan = plan_backfill(&db, &config, &svc, &opts).await.unwrap();
        assert_eq!(
            plan.learned.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![1, 2],
            "rated first, then published; article 3 needs --all"
        );
        assert_eq!(plan.interests, vec!["Writerdeck".to_string()]);
        assert!(plan.others.is_empty());
        assert!(plan.estimated_tokens > 0);

        let outcome = run_backfill(&svc, &plan).await.unwrap();
        assert_eq!(outcome.articles_embedded, 2);
        assert_eq!(outcome.interests_embedded, 1);
        let calls = backend.calls();
        assert!(calls >= 2);

        // Warm cache ⇒ empty plan and zero calls.
        let plan = plan_backfill(&db, &config, &svc, &opts).await.unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.cached, 3);
        run_backfill(&svc, &plan).await.unwrap();
        assert_eq!(backend.calls(), calls);

        // --all picks up the third article; --rated-only limits to the rated set.
        let all = plan_backfill(
            &db,
            &config,
            &svc,
            &BackfillOptions {
                all: true,
                ..opts.clone()
            },
        )
        .await
        .unwrap();
        assert_eq!(all.others.iter().map(|a| a.id).collect::<Vec<_>>(), vec![3]);
        let (_dir2, fresh_db) = db_with_articles(&[1, 2]).await;
        sqlx::query(
            "INSERT INTO rating_events (article_id, kind, source, label, value, event_at)
             VALUES (1, 'explicit', 'cli', 'loved', 1.0, ?);",
        )
        .bind(fmt_ts(Timestamp::now()))
        .execute(fresh_db.pool())
        .await
        .unwrap();
        let fresh_svc = service(
            fresh_db.clone(),
            config.voyage.clone(),
            Arc::new(MockBackend::auto(4)),
        );
        let rated_only = plan_backfill(
            &fresh_db,
            &config,
            &fresh_svc,
            &BackfillOptions {
                rated_only: true,
                all: true,
                days: 30,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rated_only.learned.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![1]
        );
        assert!(rated_only.others.is_empty());
    }
}
