//! Typed configuration (spec §3.14).
//!
//! Load order, later wins: built-in defaults ← `config.toml` (path from `--config`,
//! else `./config.toml` if present) ← `DAILY_EPUB_*` environment variables, where
//! nesting is expressed with a double underscore (`DAILY_EPUB_MINIFLUX__API_KEY`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

/// Environment-variable prefix for every override (§3.14).
pub const ENV_PREFIX: &str = "DAILY_EPUB_";
/// Nesting separator inside env var names.
pub const ENV_SPLIT: &str = "__";
/// Default config file looked up when `--config` is not given.
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Figment(#[from] Box<figment::Error>),
    #[error("config file not found: {0}")]
    Missing(PathBuf),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Figment(Box::new(e))
    }
}

/// Legacy/alternate env var for the rating-link HMAC key (spec §1).
pub const ENV_SECRET_ALIAS: &str = "DAILY_EPUB_SECRET";

/// Root configuration document (§3.14).
///
/// Unknown *top-level* keys are ignored on purpose: the prefix `DAILY_EPUB_` is
/// shared with plain operator env vars such as [`ENV_SECRET_ALIAS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// IANA tz used for day boundaries and `--date` (§3.14, notes §2).
    pub timezone: String,
    /// Ingest window size in hours (§3.1).
    pub lookback_hours: u32,
    /// Soft target for the lineup size (§13); `curation.max_article_count`
    /// is the ceiling and there is no minimum.
    pub target_article_count: usize,
    /// Days of published EPUBs kept in `publish.epub_dir` (§3.11).
    pub retention_days: u32,
    /// How many XTC issues to keep in `publish.xtc_dir` (§3.11).
    ///
    /// Counted, not dated, because an XTCH issue is ~80–100 MB of pre-rendered
    /// page bitmaps: the constraint is disk, not age.
    pub xtc_retention_count: u32,
    /// Include the Wikipedia Current Events section (§3.8).
    pub world_briefing: bool,

    /// SQLite file; parent dirs are created on open.
    pub database_path: PathBuf,
    /// Default artifact output directory (overridden by `generate --out`).
    pub out_dir: PathBuf,
    /// Scour interests OPML used to seed the taste profile (§3.6).
    pub interests_opml: PathBuf,
    /// Hand-maintained reader profile loaded for every curation run (§8.2).
    pub profile_path: PathBuf,

    pub miniflux: MinifluxConfig,
    /// Which named provider plays each LLM role, plus the role-level knobs.
    pub llm: LlmConfig,
    /// The provider registry: `[providers.<name>]`, referenced by name from
    /// `[llm]`. Keys arrive only through `DAILY_EPUB_PROVIDERS__<NAME>__API_KEY`.
    pub providers: BTreeMap<String, ProviderConfig>,
    pub voyage: VoyageConfig,
    pub curation: CurationConfig,
    pub editorial: EditorialConfig,
    pub publish: PublishConfig,
    pub xtc: XtcConfig,
    pub server: ServerConfig,
    pub bookorbit: BookorbitConfig,
    pub mail: MailConfig,
    /// `[discovery]` — propose new feed subscriptions from aggregator hits.
    pub discovery: DiscoveryConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timezone: "America/New_York".into(),
            lookback_hours: 26,
            target_article_count: 20,
            retention_days: 21,
            xtc_retention_count: 5,
            world_briefing: true,
            database_path: PathBuf::from("/var/lib/daily-epub/daily-epub.db"),
            out_dir: PathBuf::from("/var/lib/daily-epub/out"),
            interests_opml: PathBuf::from("data/scour-interests.opml"),
            profile_path: PathBuf::from("data/profile.md"),
            miniflux: MinifluxConfig::default(),
            llm: LlmConfig::default(),
            providers: default_providers(),
            voyage: VoyageConfig::default(),
            curation: CurationConfig::default(),
            editorial: EditorialConfig::default(),
            publish: PublishConfig::default(),
            xtc: XtcConfig::default(),
            server: ServerConfig::default(),
            bookorbit: BookorbitConfig::default(),
            mail: MailConfig::default(),
            discovery: DiscoveryConfig::default(),
        }
    }
}

/// `[miniflux]` — API client settings (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MinifluxConfig {
    pub base_url: String,
    /// `X-Auth-Token`; supply via `DAILY_EPUB_MINIFLUX__API_KEY`.
    pub api_key: Option<String>,
    /// Page size for `GET /v1/entries` (Miniflux caps this at 250).
    pub page_limit: u32,
}

impl Default for MinifluxConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8082".into(),
            api_key: None,
            page_limit: 250,
        }
    }
}

/// `[llm]` — the role assignments and the role-level knobs (§4).
///
/// `bulk` runs triage, deep assessment and every fallback; `editor` runs the
/// lineup, summaries, the Brief and the profile rebuild. Both name an entry of
/// `[providers.*]`; an empty `editor` means "everything runs on bulk".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmConfig {
    pub bulk: String,
    pub editor: String,
    /// Articles per first-pass triage request (§10).
    pub triage_batch_size: usize,
    /// Articles per close-reading assessment request (§12.1).
    pub deep_batch_size: usize,
    /// Sent only by providers that accept a temperature (`kind = "openai"`).
    pub score_temperature: f32,
    pub editorial_temperature: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            bulk: "deepseek".into(),
            editor: "anthropic".into(),
            triage_batch_size: 25,
            deep_batch_size: 8,
            score_temperature: 0.3,
            editorial_temperature: 0.8,
        }
    }
}

impl LlmConfig {
    /// The bulk provider's name, `None` when `bulk = ""`.
    pub fn bulk_name(&self) -> Option<&str> {
        Some(self.bulk.trim()).filter(|name| !name.is_empty())
    }

    /// The editor provider's name, `None` when `editor` is empty or absent.
    pub fn editor_name(&self) -> Option<&str> {
        Some(self.editor.trim()).filter(|name| !name.is_empty())
    }

    /// `(role, provider name)` for every assigned role, bulk first.
    pub fn roles(&self) -> Vec<(&'static str, &str)> {
        let mut roles = Vec::with_capacity(2);
        if let Some(name) = self.bulk_name() {
            roles.push(("bulk", name));
        }
        if let Some(name) = self.editor_name() {
            roles.push(("editor", name));
        }
        roles
    }
}

/// The wire protocol a provider speaks (`[providers.<name>] kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenAI-compatible `POST {base_url}/chat/completions` (DeepSeek, Gemini's
    /// compatibility endpoint, OpenAI itself).
    OpenAi,
    /// The Anthropic Messages API.
    Anthropic,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
        }
    }
}

/// `[providers.<name>]` — one chat-completion provider: endpoint, model,
/// reasoning effort, its own daily ceiling and its price table (§4, §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    /// Supply only via `DAILY_EPUB_PROVIDERS__<NAME>__API_KEY`.
    pub api_key: Option<String>,
    /// `kind = "anthropic"`: `output_config.effort` (`low | medium | high |
    /// xhigh | max`). `kind = "openai"`: passed through as `reasoning_effort`.
    pub effort: Option<String>,
    /// Spend ceiling per UTC day; `0` disables the guard.
    pub max_daily_usd: f64,
    pub max_concurrent_requests: usize,
    /// USD per 1M cache-miss input tokens.
    pub price_input_per_mtok: f64,
    /// USD per 1M cache-hit input tokens.
    pub price_cache_read_per_mtok: f64,
    /// USD per 1M tokens written to the prompt cache (0 where caching is implicit).
    pub price_cache_write_per_mtok: f64,
    /// USD per 1M output tokens (thinking tokens included where billed as output).
    pub price_output_per_mtok: f64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            effort: None,
            max_daily_usd: 0.0,
            max_concurrent_requests: 4,
            price_input_per_mtok: 0.0,
            price_cache_read_per_mtok: 0.0,
            price_cache_write_per_mtok: 0.0,
            price_output_per_mtok: 0.0,
        }
    }
}

impl ProviderConfig {
    /// DeepSeek V4 Flash over its OpenAI-compatible endpoint (verified 2026-08-15).
    pub fn deepseek() -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
            api_key: None,
            effort: None,
            max_daily_usd: 2.0,
            max_concurrent_requests: 4,
            price_input_per_mtok: 0.14,
            price_cache_read_per_mtok: 0.0028,
            price_cache_write_per_mtok: 0.0,
            price_output_per_mtok: 0.28,
        }
    }

    /// Claude Opus 5 over the Messages API (verified 2026-09-02).
    pub fn anthropic() -> Self {
        Self {
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-5".into(),
            api_key: None,
            effort: Some("high".into()),
            max_daily_usd: 3.0,
            max_concurrent_requests: 4,
            price_input_per_mtok: 5.0,
            price_cache_read_per_mtok: 0.5,
            price_cache_write_per_mtok: 6.25,
            price_output_per_mtok: 25.0,
        }
    }

    /// Gemini 3.8 Flash over Google's OpenAI-compatible endpoint (verified
    /// 2026-09-02; promotional prices through 2026-12-31).
    pub fn gemini() -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            model: "gemini-3.8-flash".into(),
            api_key: None,
            effort: Some("high".into()),
            max_daily_usd: 3.0,
            max_concurrent_requests: 4,
            price_input_per_mtok: 0.75,
            price_cache_read_per_mtok: 0.075,
            price_cache_write_per_mtok: 0.0,
            price_output_per_mtok: 3.75,
        }
    }

    /// The only place a key may come from: `DAILY_EPUB_PROVIDERS__<NAME>__API_KEY`.
    pub fn api_key_env_var(name: &str) -> String {
        format!(
            "{ENV_PREFIX}PROVIDERS{ENV_SPLIT}{}{ENV_SPLIT}API_KEY",
            name.to_uppercase()
        )
    }

    /// The key, trimmed, when one is configured.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }

    /// A copy safe to log or persist: the key is stripped.
    pub fn redacted(&self) -> Self {
        Self {
            api_key: None,
            ..self.clone()
        }
    }
}

/// The three providers `config.example.toml` documents.
pub fn default_providers() -> BTreeMap<String, ProviderConfig> {
    BTreeMap::from([
        ("deepseek".to_string(), ProviderConfig::deepseek()),
        ("anthropic".to_string(), ProviderConfig::anthropic()),
        ("gemini".to_string(), ProviderConfig::gemini()),
    ])
}

/// Which provider writes per-article summaries (§14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummaryModel {
    Editor,
    Bulk,
}

/// `[editorial]` — summary provider and per-article input budget (§14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EditorialConfig {
    pub summary_model: SummaryModel,
    pub summary_input_tokens: usize,
}

impl Default for EditorialConfig {
    fn default() -> Self {
        Self {
            summary_model: SummaryModel::Editor,
            summary_input_tokens: 3_000,
        }
    }
}

/// `[voyage]` — embedding endpoint and cache shape (§4.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VoyageConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    /// Supply via `DAILY_EPUB_VOYAGE__API_KEY`; never put it in the TOML.
    pub api_key: Option<String>,
    pub output_dimension: usize,
    pub batch_size: usize,
    pub max_concurrent_requests: usize,
    pub max_input_chars: usize,
    pub max_daily_usd: f64,
}

impl Default for VoyageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "https://api.voyageai.com/v1".into(),
            model: "voyage-4-lite".into(),
            api_key: None,
            output_dimension: 512,
            batch_size: 32,
            max_concurrent_requests: 4,
            max_input_chars: 60_000,
            max_daily_usd: 0.50,
        }
    }
}

/// `[curation]` — hygiene, feedback weights, the ranker and the section palette (§19).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CurationConfig {
    /// Absolute issue-size ceiling; the editor has no minimum (§13).
    pub max_article_count: usize,
    pub recent_rejection_days: i64,
    pub recent_rejection_floor: f64,
    /// Miniflux feed ids or site URLs that can never be dropped (§3.5).
    pub always_include_feeds: Vec<String>,
    /// Hosts excluded outright (§3.5).
    pub blocked_domains: Vec<String>,
    /// Extra paywalled hosts, merged with [`crate::extract::DEFAULT_PAYWALL_DOMAINS`]
    /// by the extraction stage's `excerpt_only` heuristic (§3.3).
    pub paywall_domains: Vec<String>,
    /// The only section names the editor may use (§13).
    pub sections: Vec<String>,
    pub feedback: FeedbackConfig,
    pub ranking: RankingConfig,
}

impl Default for CurationConfig {
    fn default() -> Self {
        Self {
            max_article_count: 28,
            recent_rejection_days: 7,
            recent_rejection_floor: 3.0,
            always_include_feeds: Vec::new(),
            blocked_domains: Vec::new(),
            paywall_domains: Vec::new(),
            sections: [
                "Top Stories",
                "Tech & Engineering",
                "Science & Space",
                "AI & Machine Learning",
                "Culture & Essays",
                "Boston & Local",
                "Niche Corner",
                "From the Blogroll",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            feedback: FeedbackConfig::default(),
            ranking: RankingConfig::default(),
        }
    }
}

/// `[curation.ranking]` — every weight, quota, gate and threshold of the
/// personalized ranker (plan §19).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RankingConfig {
    pub triage_max: usize,
    pub deep_keep: usize,
    pub shortlist_keep: usize,
    pub assessment_reuse_days: i64,
    pub rating_lookback_days: i64,
    pub rating_half_life_days: f64,
    pub neighbour_k: usize,
    pub negative_coefficient: f64,
    pub knn_floor: usize,
    pub knn_full: usize,
    pub feed_floor: usize,
    pub feed_full: usize,
    /// Fraction of the preliminary blend and the utility removed from any
    /// candidate whose author has a current *AI slop* verdict (§9.3). `1.0`
    /// zeroes such candidates; `0.0` disables the penalty.
    pub slop_author_penalty: f64,
    pub semantic_min_words: i64,
    pub exploration_slots: usize,
    pub embedding_retention_days: i64,
    pub telemetry_retention_days: i64,
    pub quotas: RankingQuotas,
    pub weights: RankingWeights,
    pub diversity: DiversityConfig,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            triage_max: 800,
            deep_keep: 120,
            shortlist_keep: 60,
            assessment_reuse_days: 3,
            rating_lookback_days: 180,
            rating_half_life_days: 60.0,
            neighbour_k: 5,
            negative_coefficient: 0.75,
            knn_floor: 8,
            knn_full: 25,
            feed_floor: 15,
            feed_full: 40,
            slop_author_penalty: 0.75,
            semantic_min_words: 300,
            exploration_slots: 5,
            embedding_retention_days: 120,
            telemetry_retention_days: 180,
            quotas: RankingQuotas::default(),
            weights: RankingWeights::default(),
            diversity: DiversityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RankingQuotas {
    pub triage: usize,
    pub interest: usize,
    pub knn: usize,
}

impl Default for RankingQuotas {
    fn default() -> Self {
        Self {
            triage: 60,
            interest: 20,
            knn: 20,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RankingWeights {
    pub preliminary: PreliminaryWeights,
    pub utility: UtilityWeights,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PreliminaryWeights {
    pub interest: f64,
    pub knn: f64,
    pub heuristic: f64,
    pub feed: f64,
    pub social: f64,
}

impl Default for PreliminaryWeights {
    fn default() -> Self {
        Self {
            interest: 0.35,
            knn: 0.25,
            heuristic: 0.20,
            feed: 0.10,
            social: 0.10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UtilityWeights {
    pub quality: f64,
    pub fit: f64,
    pub knn: f64,
    pub interest: f64,
    pub feed: f64,
    pub triage: f64,
    pub social: f64,
    pub heuristic: f64,
}

impl Default for UtilityWeights {
    fn default() -> Self {
        Self {
            quality: 0.40,
            fit: 0.20,
            knn: 0.15,
            interest: 0.10,
            feed: 0.05,
            triage: 0.05,
            social: 0.03,
            heuristic: 0.02,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiversityConfig {
    pub cluster_threshold: f64,
    pub per_cluster_cap: usize,
    pub utility_protected: usize,
}

impl Default for DiversityConfig {
    fn default() -> Self {
        Self {
            cluster_threshold: 0.85,
            per_cluster_cap: 2,
            utility_protected: 10,
        }
    }
}

/// `[curation.feedback]` — explicit verdict weights and prompt history (§6, §8.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FeedbackConfig {
    pub loved_value: f64,
    pub good_value: f64,
    pub not_for_me_value: f64,
    /// Weight of an *AI slop* verdict. The author penalty is separate
    /// (`ranking.slop_author_penalty`).
    pub slop_value: f64,
    pub verdicts_in_prompt: usize,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            loved_value: 1.0,
            good_value: 0.35,
            not_for_me_value: -1.0,
            slop_value: -1.0,
            verdicts_in_prompt: 60,
        }
    }
}

/// `[publish]` — where finished artifacts land (§3.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PublishConfig {
    /// Where both EPUB editions land: the source of the OPDS feed, served at
    /// `/files/epub/`, and a BookOrbit watched folder if one is configured.
    ///
    /// Renamed from `bookorbit_dir` once the built-in feed started serving this
    /// directory directly — BookOrbit is optional, the directory is not.
    pub epub_dir: PathBuf,
    /// Directory served at `/files/xtc/`. Not listed in the OPDS feed (§3.11).
    pub xtc_dir: PathBuf,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            epub_dir: PathBuf::from("/srv/bookorbit/libraries/daily-epub"),
            xtc_dir: PathBuf::from("/var/lib/daily-epub/xtc"),
        }
    }
}

/// XTC output flavour (§3.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XtcFormat {
    /// 1-bit.
    Xtc,
    /// 2-bit grayscale — the default (better image quality).
    Xtch,
}

impl XtcFormat {
    /// Value passed to the converter's `-f` flag.
    pub fn as_str(self) -> &'static str {
        match self {
            XtcFormat::Xtc => "xtc",
            XtcFormat::Xtch => "xtch",
        }
    }

    /// File extension of the produced artifact.
    pub fn extension(self) -> &'static str {
        self.as_str()
    }
}

/// `[xtc]` — invocation of `epub-to-xtc-converter` (§3.11, notes "verified facts").
///
/// The converter has no global npm bin, so `command` + `args` form the prefix and
/// the code appends `<input.epub> -o <output> -f <format>` (plus `-c <settings>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct XtcConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    pub format: XtcFormat,
    /// Optional settings JSON passed as `-c`.
    pub settings: Option<PathBuf>,
}

impl Default for XtcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: "node".into(),
            args: vec![
                "/opt/epub-to-xtc-converter/cli/index.js".into(),
                "convert".into(),
            ],
            format: XtcFormat::Xtch,
            settings: None,
        }
    }
}

/// `[server]` — axum listener and rating-link signing (§3.9, §3.12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub bind: String,
    /// Base URL rating links are built from.
    pub public_url: String,
    /// HMAC key for rating tokens; supply via `DAILY_EPUB_SERVER__HMAC_SECRET`.
    pub hmac_secret: Option<String>,
    /// Optional Basic auth for `/opds/*` and `/files/*`.
    pub basic_auth_user: Option<String>,
    pub basic_auth_pass: Option<String>,
    /// Sliding web-session lifetime in days.
    pub session_days: u32,
    /// Login and access-request POSTs allowed per IP during the configured window.
    pub login_attempts: u32,
    pub login_window_minutes: u32,
    /// Whether the operator dashboard may start systemd jobs.
    pub jobs_enabled: bool,
    /// Number of journal lines displayed for a job.
    pub journal_lines: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3499".into(),
            public_url: "https://daily.hallada.net".into(),
            hmac_secret: None,
            basic_auth_user: None,
            basic_auth_pass: None,
            session_days: 30,
            login_attempts: 10,
            login_window_minutes: 15,
            jobs_enabled: true,
            journal_lines: 300,
        }
    }
}

/// `[bookorbit]` — optional web-reader integration via BookOrbit's OPDS API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BookorbitConfig {
    /// Whether the signed-in BookOrbit reader integration is enabled.
    pub enabled: bool,
    /// Base URL opened in the reader's browser.
    pub public_url: String,
    /// Base URL used for server-side OPDS requests.
    pub api_url: String,
    /// Dedicated BookOrbit OPDS username.
    pub opds_user: Option<String>,
    /// Dedicated BookOrbit OPDS password; supply via
    /// `DAILY_EPUB_BOOKORBIT__OPDS_PASS`.
    pub opds_pass: Option<String>,
}

impl Default for BookorbitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            public_url: "https://bookorbit.hallada.net".into(),
            api_url: "http://127.0.0.1:3498".into(),
            opds_user: None,
            opds_pass: None,
        }
    }
}

impl BookorbitConfig {
    /// Whether the integration is enabled and has non-empty OPDS credentials.
    pub fn is_active(&self) -> bool {
        self.enabled
            && self
                .opds_user
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && self
                .opds_pass
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    /// Browser-facing base URL without trailing slashes.
    pub fn public_url(&self) -> &str {
        self.public_url.trim_end_matches('/')
    }

    /// Server-facing API base URL without trailing slashes.
    pub fn api_url(&self) -> &str {
        self.api_url.trim_end_matches('/')
    }
}

/// `[mail]` — optional outbound SMTP delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MailConfig {
    /// Whether outbound mail is enabled.
    pub enabled: bool,
    /// SMTP relay hostname.
    pub smtp_host: String,
    /// SMTP relay port.
    pub smtp_port: u16,
    /// Upgrade the connection with STARTTLS; false uses implicit TLS.
    pub smtp_starttls: bool,
    /// SMTP username.
    pub smtp_user: Option<String>,
    /// SMTP password; supply via `DAILY_EPUB_MAIL__SMTP_PASS`.
    pub smtp_pass: Option<String>,
    /// Sender mailbox, either an address or `Name <address>`.
    pub from: String,
    /// Recipient for access-request notifications.
    pub notify_to: Option<String>,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_starttls: true,
            smtp_user: None,
            smtp_pass: None,
            from: String::new(),
            notify_to: None,
        }
    }
}

/// `[discovery]` — feed discovery from aggregator-only articles (feed
/// discovery plan §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscoveryConfig {
    /// Whether the discovery stage runs during `generate`.
    pub enabled: bool,
    /// How many not-yet-checked hosts one run may look up in Miniflux.
    pub max_lookups_per_run: usize,
    /// Hosts never looked up (aggregators, code hosts, social networks).
    pub skip_hosts: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lookups_per_run: 30,
            skip_hosts: [
                "news.ycombinator.com",
                "lobste.rs",
                "reddit.com",
                "github.com",
                "gist.github.com",
                "x.com",
                "twitter.com",
                "youtube.com",
                "en.wikipedia.org",
                "arxiv.org",
                "docs.google.com",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

impl MailConfig {
    /// Whether mail is enabled with all fields required for SMTP delivery.
    pub fn is_active(&self) -> bool {
        self.enabled
            && !self.smtp_host.trim().is_empty()
            && !self.from.trim().is_empty()
            && self
                .smtp_user
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && self
                .smtp_pass
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}

/// Config keys that moved from `[deepseek]` to `[llm]`; anywhere else they are
/// a stale-configuration error.
const LLM_ROLE_KEYS: &[&str] = &[
    "deep_batch_size",
    "triage_batch_size",
    "score_temperature",
    "editorial_temperature",
];

/// The pre-registry provider tables; each is now `[providers.<name>]`.
const STALE_PROVIDER_TABLES: &[&str] = &["deepseek", "anthropic"];

impl Config {
    /// Build the figment layer stack. `path` is required to exist when explicit.
    fn figment(path: Option<&Path>, require_file: bool) -> Result<Figment, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if let Some(p) = path {
            if require_file && !p.exists() {
                return Err(ConfigError::Missing(p.to_path_buf()));
            }
            if p.exists() {
                fig = fig.merge(Toml::file(p));
            }
        }
        Ok(fig.merge(Env::prefixed(ENV_PREFIX).split(ENV_SPLIT)))
    }

    /// The file `load` reads: the explicit `--config` path, else `./config.toml`
    /// when it exists, else `None` (built-in defaults plus the environment).
    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        match explicit {
            Some(path) => Some(path.to_path_buf()),
            None => Some(PathBuf::from(DEFAULT_CONFIG_FILE)).filter(|path| path.exists()),
        }
    }

    /// Load config for the CLI: explicit `--config` path, else `./config.toml`
    /// when it exists, then `DAILY_EPUB_*` env overrides (§3.14).
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        if let Some(message) =
            stale_env_error(std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()))
        {
            return Err(ConfigError::Invalid(message));
        }
        let (path, require) = match explicit {
            Some(p) => (Some(p.to_path_buf()), true),
            None => (Some(PathBuf::from(DEFAULT_CONFIG_FILE)), false),
        };
        if let Some(path) = path.as_deref().filter(|path| path.exists()) {
            let raw = std::fs::read_to_string(path).map_err(|error| {
                ConfigError::Invalid(format!("could not inspect {}: {error}", path.display()))
            })?;
            if let Some(message) = stale_toml_error(&raw) {
                return Err(ConfigError::Invalid(message));
            }
        }
        let mut config: Config = Self::figment(path.as_deref(), require)?.extract()?;
        // §1 tells the operator to set `DAILY_EPUB_SECRET`; §3.14 calls the key
        // `server.hmac_secret`. Accept both, with the explicit key winning.
        if config.server.hmac_secret.is_none() {
            config.server.hmac_secret = std::env::var(ENV_SECRET_ALIAS)
                .ok()
                .filter(|v| !v.is_empty());
        }
        config.validate()?;
        Ok(config)
    }

    /// The provider an `[llm]` role names, with its name.
    fn role_provider<'a>(&'a self, name: Option<&'a str>) -> Option<(&'a str, &'a ProviderConfig)> {
        let name = name?;
        self.providers.get(name).map(|provider| (name, provider))
    }

    /// `(name, provider)` for `llm.bulk`, `None` when no bulk provider is set.
    pub fn bulk_provider(&self) -> Option<(&str, &ProviderConfig)> {
        self.role_provider(self.llm.bulk_name())
    }

    /// `(name, provider)` for `llm.editor`, `None` when there is no editor.
    pub fn editor_provider(&self) -> Option<(&str, &ProviderConfig)> {
        self.role_provider(self.llm.editor_name())
    }

    /// Every provider some role references, bulk first, each once.
    pub fn referenced_providers(&self) -> Vec<(&str, &ProviderConfig)> {
        let mut seen = Vec::new();
        for (_, name) in self.llm.roles() {
            if seen.iter().any(|(seen, _)| *seen == name) {
                continue;
            }
            if let Some(provider) = self.providers.get(name) {
                seen.push((name, provider));
            }
        }
        seen
    }

    /// The registry with every key stripped, for logs and `runs.config_json`.
    pub fn providers_redacted(&self) -> BTreeMap<String, ProviderConfig> {
        self.providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.redacted()))
            .collect()
    }

    /// `config check`: one fact per line about the resolved configuration.
    ///
    /// Lines that need the operator's attention (a missing key or file) start
    /// with `! `; nothing here opens the database, takes the lock or touches
    /// the network, and no key is ever printed.
    pub fn check_report(&self, path: Option<&Path>) -> Vec<String> {
        fn exists(path: &Path) -> &'static str {
            if path.exists() { "exists" } else { "MISSING" }
        }
        fn file_line(label: &str, path: &Path) -> String {
            let prefix = if path.exists() { "" } else { "! " };
            format!("{prefix}{label}: {} ({})", path.display(), exists(path))
        }
        fn provider_line(label: &str, name: &str, provider: &ProviderConfig) -> String {
            let key = if provider.api_key().is_some() {
                "key present".to_string()
            } else {
                format!(
                    "key MISSING (set {})",
                    ProviderConfig::api_key_env_var(name)
                )
            };
            let prefix = if provider.api_key().is_some() {
                ""
            } else {
                "! "
            };
            format!(
                "{prefix}{label}: {name} · {} · {} · effort {} · max_daily_usd ${:.2} · {key}",
                provider.kind.as_str(),
                provider.model,
                provider.effort.as_deref().unwrap_or("-"),
                provider.max_daily_usd,
            )
        }

        let mut lines = Vec::new();
        lines.push(match path {
            Some(path) => format!("config: {}", path.display()),
            None => "config: built-in defaults (no config.toml; environment only)".into(),
        });
        lines.push(file_line("database_path", &self.database_path));
        lines.push(file_line("profile_path", &self.profile_path));
        lines.push(file_line("interests_opml", &self.interests_opml));
        for (role, name) in self.llm.roles() {
            match self.providers.get(name) {
                Some(provider) => lines.push(provider_line(&format!("llm.{role}"), name, provider)),
                None => lines.push(format!("! llm.{role}: {name} is not a [providers.*] entry")),
            }
        }
        if self.llm.bulk_name().is_none() {
            lines.push("! llm.bulk: none (triage and deep assessment are skipped)".into());
        }
        if self.llm.editor_name().is_none() {
            lines.push("llm.editor: none (editor work runs on the bulk provider)".into());
        }
        let referenced = self.referenced_providers();
        for (name, provider) in &self.providers {
            if referenced.iter().any(|(used, _)| used == name) {
                continue;
            }
            let key = if provider.api_key().is_some() {
                "key present"
            } else {
                "key absent"
            };
            lines.push(format!(
                "providers.{name}: unreferenced · {} · {} · {key}",
                provider.kind.as_str(),
                provider.model
            ));
        }
        let voyage_key = self
            .voyage
            .api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty());
        lines.push(format!(
            "{}voyage: {} · {} · max_daily_usd ${:.2} · {}",
            if voyage_key || !self.voyage.enabled {
                ""
            } else {
                "! "
            },
            self.voyage.model,
            if self.voyage.enabled {
                "enabled"
            } else {
                "disabled"
            },
            self.voyage.max_daily_usd,
            if voyage_key {
                "key present".to_string()
            } else {
                format!("key MISSING (set {ENV_PREFIX}VOYAGE{ENV_SPLIT}API_KEY)")
            }
        ));
        lines.push(format!(
            "editorial.summary_model: {}",
            match self.editorial.summary_model {
                SummaryModel::Editor => "editor",
                SummaryModel::Bulk => "bulk",
            }
        ));
        lines.push(file_line("publish.epub_dir", &self.publish.epub_dir));
        lines.push(file_line("publish.xtc_dir", &self.publish.xtc_dir));
        lines
    }

    /// Cheap sanity checks so misconfiguration fails at startup, not mid-run.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.mail.enabled && self.mail.smtp_host.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "mail.smtp_host must not be empty when mail.enabled is true".into(),
            ));
        }
        if self.mail.enabled && self.mail.from.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "mail.from must not be empty when mail.enabled is true".into(),
            ));
        }
        if self.server.session_days == 0 {
            return Err(ConfigError::Invalid(
                "server.session_days must be >= 1".into(),
            ));
        }
        if self.server.login_attempts == 0 {
            return Err(ConfigError::Invalid(
                "server.login_attempts must be >= 1".into(),
            ));
        }
        if self.server.login_window_minutes == 0 {
            return Err(ConfigError::Invalid(
                "server.login_window_minutes must be >= 1".into(),
            ));
        }
        if !(10..=5000).contains(&self.server.journal_lines) {
            return Err(ConfigError::Invalid(
                "server.journal_lines must be between 10 and 5000".into(),
            ));
        }
        if self.lookback_hours == 0 {
            return Err(ConfigError::Invalid("lookback_hours must be > 0".into()));
        }
        if self.target_article_count == 0 {
            return Err(ConfigError::Invalid(
                "target_article_count must be > 0".into(),
            ));
        }
        if self.curation.max_article_count < self.target_article_count {
            return Err(ConfigError::Invalid(
                "curation.max_article_count must be >= target_article_count".into(),
            ));
        }
        if self.llm.deep_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "llm.deep_batch_size must be >= 1".into(),
            ));
        }
        if self.llm.triage_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "llm.triage_batch_size must be >= 1".into(),
            ));
        }
        if self.editorial.summary_input_tokens == 0 {
            return Err(ConfigError::Invalid(
                "editorial.summary_input_tokens must be >= 1".into(),
            ));
        }
        for (role, name) in self.llm.roles() {
            if !self.providers.contains_key(name) {
                return Err(ConfigError::Invalid(format!(
                    "llm.{role} = {name:?} names no [providers.{name}] entry"
                )));
            }
        }
        for (name, provider) in &self.providers {
            provider.validate(name)?;
        }
        let ranking = &self.curation.ranking;
        if ranking.deep_keep < ranking.shortlist_keep
            || ranking.shortlist_keep < self.target_article_count
        {
            return Err(ConfigError::Invalid(
                "curation.ranking must satisfy deep_keep >= shortlist_keep >= target_article_count"
                    .into(),
            ));
        }
        if ranking.knn_full <= ranking.knn_floor || ranking.feed_full <= ranking.feed_floor {
            return Err(ConfigError::Invalid(
                "curation.ranking *_full must be > *_floor >= 0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&ranking.slop_author_penalty) {
            return Err(ConfigError::Invalid(
                "curation.ranking.slop_author_penalty must be between 0 and 1".into(),
            ));
        }
        if !(0.0..=1.0).contains(&ranking.diversity.cluster_threshold) {
            return Err(ConfigError::Invalid(
                "curation.ranking.diversity.cluster_threshold must be between 0 and 1".into(),
            ));
        }
        if ranking.diversity.per_cluster_cap == 0 {
            return Err(ConfigError::Invalid(
                "curation.ranking.diversity.per_cluster_cap must be >= 1".into(),
            ));
        }
        let preliminary = &ranking.weights.preliminary;
        let utility = &ranking.weights.utility;
        let weights = [
            preliminary.interest,
            preliminary.knn,
            preliminary.heuristic,
            preliminary.feed,
            preliminary.social,
            utility.quality,
            utility.fit,
            utility.knn,
            utility.interest,
            utility.feed,
            utility.triage,
            utility.social,
            utility.heuristic,
        ];
        if weights
            .iter()
            .any(|weight| !weight.is_finite() || *weight < 0.0)
        {
            return Err(ConfigError::Invalid(
                "curation.ranking weights must be finite and non-negative".into(),
            ));
        }
        if self.voyage.batch_size == 0 || self.voyage.max_concurrent_requests == 0 {
            return Err(ConfigError::Invalid(
                "voyage.batch_size and voyage.max_concurrent_requests must be >= 1".into(),
            ));
        }
        if ![256, 512, 1024, 2048].contains(&self.voyage.output_dimension) {
            return Err(ConfigError::Invalid(
                "voyage.output_dimension must be one of 256, 512, 1024, 2048".into(),
            ));
        }
        if ranking.rating_half_life_days <= 0.0 || !ranking.rating_half_life_days.is_finite() {
            return Err(ConfigError::Invalid(
                "curation.ranking.rating_half_life_days must be > 0".into(),
            ));
        }
        if self.curation.sections.is_empty() {
            return Err(ConfigError::Invalid(
                "curation.sections must not be empty".into(),
            ));
        }
        self.tz()?;
        Ok(())
    }

    /// Resolve [`Config::timezone`] into a `jiff` time zone (notes §2).
    pub fn tz(&self) -> Result<jiff::tz::TimeZone, ConfigError> {
        jiff::tz::TimeZone::get(&self.timezone)
            .map_err(|e| ConfigError::Invalid(format!("unknown timezone {}: {e}", self.timezone)))
    }
}

impl ProviderConfig {
    /// Field-level checks for one `[providers.<name>]` entry.
    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid(message);
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(invalid(format!(
                "provider name {name:?} must be lowercase ascii letters, digits or '_' \
                 so that {} can reach it",
                ProviderConfig::api_key_env_var(name)
            )));
        }
        if self.base_url.trim().is_empty() {
            return Err(invalid(format!(
                "providers.{name}.base_url must not be empty"
            )));
        }
        if self.model.trim().is_empty() {
            return Err(invalid(format!("providers.{name}.model must not be empty")));
        }
        if self.max_concurrent_requests == 0 {
            return Err(invalid(format!(
                "providers.{name}.max_concurrent_requests must be >= 1"
            )));
        }
        if !self.max_daily_usd.is_finite() || self.max_daily_usd < 0.0 {
            return Err(invalid(format!(
                "providers.{name}.max_daily_usd must be >= 0"
            )));
        }
        for (key, price) in [
            ("price_input_per_mtok", self.price_input_per_mtok),
            ("price_cache_read_per_mtok", self.price_cache_read_per_mtok),
            (
                "price_cache_write_per_mtok",
                self.price_cache_write_per_mtok,
            ),
            ("price_output_per_mtok", self.price_output_per_mtok),
        ] {
            if !price.is_finite() || price < 0.0 {
                return Err(invalid(format!("providers.{name}.{key} must be >= 0")));
            }
        }
        match (self.kind, self.effort.as_deref().map(str::trim)) {
            (ProviderKind::Anthropic, Some(effort))
                if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") =>
            {
                return Err(invalid(format!(
                    "providers.{name}.effort must be one of low, medium, high, xhigh, max"
                )));
            }
            (ProviderKind::OpenAi, Some("")) => {
                return Err(invalid(format!(
                    "providers.{name}.effort must be a reasoning_effort value or absent"
                )));
            }
            _ => {}
        }
        Ok(())
    }
}

/// The stale-configuration checks over the raw TOML text, following the
/// `prefilter_keep` precedent: silently ignoring a `[deepseek]` table would
/// leave the bulk provider unconfigured and publish a heuristic paper.
fn stale_toml_error(raw: &str) -> Option<String> {
    let mut section: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim_start();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if STALE_PROVIDER_TABLES.contains(&name.as_str()) {
                return Some(format!(
                    "the [{name}] table was replaced by [providers.{name}] (kind, base_url, \
                     model, effort, max_daily_usd, max_concurrent_requests, price_*) and \
                     [llm] (bulk, editor, triage_batch_size, deep_batch_size, \
                     score_temperature, editorial_temperature); keys move to \
                     {}",
                    ProviderConfig::api_key_env_var(&name)
                ));
            }
            section = Some(name);
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key == "prefilter_keep" {
            return Some("prefilter_keep was removed; use curation.ranking.deep_keep".into());
        }
        if key == "score_batch_size" {
            return Some("deepseek.score_batch_size was removed; use llm.deep_batch_size".into());
        }
        if key == "max_daily_usd" && section.is_none() {
            return Some(
                "the top-level max_daily_usd was removed; every provider carries its own \
                 providers.<name>.max_daily_usd"
                    .into(),
            );
        }
        if LLM_ROLE_KEYS.contains(&key) && section.as_deref() != Some("llm") {
            return Some(format!(
                "{key} moved to the [llm] table (it was {}.{key})",
                section.as_deref().unwrap_or("top-level")
            ));
        }
    }
    None
}

/// A `DAILY_EPUB_DEEPSEEK__*` or `DAILY_EPUB_ANTHROPIC__*` variable in the
/// environment, naming the variable that replaced it. A silently unavailable
/// bulk provider would publish a heuristic paper, so this fails config load.
fn stale_env_error(vars: impl IntoIterator<Item = String>) -> Option<String> {
    for var in vars {
        let Some(rest) = var.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        for table in STALE_PROVIDER_TABLES {
            let Some(key) = rest.strip_prefix(&format!("{}{ENV_SPLIT}", table.to_uppercase()))
            else {
                continue;
            };
            let replacement = if key == "SCORE_BATCH_SIZE" {
                format!("{ENV_PREFIX}LLM{ENV_SPLIT}DEEP_BATCH_SIZE")
            } else if LLM_ROLE_KEYS.contains(&key.to_lowercase().as_str()) {
                format!("{ENV_PREFIX}LLM{ENV_SPLIT}{key}")
            } else {
                format!(
                    "{ENV_PREFIX}PROVIDERS{ENV_SPLIT}{}{ENV_SPLIT}{key}",
                    table.to_uppercase()
                )
            };
            return Some(format!(
                "{var} is stale: the [{table}] table became [providers.{table}]; set {replacement}"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;

    /// `Config::load` reads the process environment and `toml_then_env_layering`
    /// writes it, so every test that does either takes this lock; tests run in
    /// parallel threads and the environment is shared.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.timezone, "America/New_York");
        assert_eq!(c.lookback_hours, 26);
        assert_eq!(c.target_article_count, 20);
        assert_eq!(c.curation.ranking.deep_keep, 120);
        assert_eq!(c.retention_days, 21);
        assert!(c.world_briefing);
        assert_eq!(c.llm.bulk, "deepseek");
        assert_eq!(c.llm.editor, "anthropic");
        assert_eq!(c.llm.triage_batch_size, 25);
        assert_eq!(c.llm.deep_batch_size, 8);
        assert_eq!(c.providers["deepseek"].model, "deepseek-v4-flash");
        assert_eq!(c.providers["deepseek"].max_daily_usd, 2.0);
        assert_eq!(c.providers["anthropic"].kind, ProviderKind::Anthropic);
        assert_eq!(c.providers["gemini"].kind, ProviderKind::OpenAi);
        assert_eq!(c.providers.len(), 3);
        assert_eq!(c.curation.recent_rejection_days, 7);
        assert_eq!(c.curation.recent_rejection_floor, 3.0);
        assert_eq!(c.profile_path, PathBuf::from("data/profile.md"));
        assert_eq!(c.curation.feedback.good_value, 0.35);
        assert_eq!(c.curation.feedback.slop_value, -1.0);
        assert_eq!(c.curation.ranking.slop_author_penalty, 0.75);
        assert_eq!(c.curation.feedback.verdicts_in_prompt, 60);
        assert_eq!(c.xtc.format, XtcFormat::Xtch);
        assert_eq!(c.curation.sections.len(), 8);
        assert!(!c.bookorbit.enabled);
        assert_eq!(c.bookorbit.public_url, "https://bookorbit.hallada.net");
        assert_eq!(c.bookorbit.api_url, "http://127.0.0.1:3498");
        assert!(c.bookorbit.opds_user.is_none());
        assert!(c.bookorbit.opds_pass.is_none());
        assert!(!c.mail.enabled);
        assert!(c.mail.smtp_host.is_empty());
        assert_eq!(c.mail.smtp_port, 587);
        assert!(c.mail.smtp_starttls);
        assert!(c.mail.smtp_user.is_none());
        assert!(c.mail.smtp_pass.is_none());
        assert!(c.mail.from.is_empty());
        assert!(c.mail.notify_to.is_none());
        c.validate().unwrap();
    }

    #[test]
    // `Jail::expect_with` dictates the closure's `figment::Error` return type.
    #[allow(clippy::result_large_err)]
    fn toml_then_env_layering() {
        let _env = env_guard();
        Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                lookback_hours = 30
                world_briefing = false

                [miniflux]
                base_url = "http://127.0.0.1:9999"

                [curation]
                sections = ["Top Stories", "Niche Corner"]

                [xtc]
                command = "node"
                args = ["/opt/epub-to-xtc-converter/cli/index.js", "convert"]
                format = "xtc"
                "#,
            )?;
            jail.set_env("DAILY_EPUB_MINIFLUX__API_KEY", "secret-token");
            jail.set_env("DAILY_EPUB_TARGET_ARTICLE_COUNT", "12");
            jail.set_env("DAILY_EPUB_SERVER__HMAC_SECRET", "hunter2");
            jail.set_env("DAILY_EPUB_BOOKORBIT__OPDS_PASS", "orbit-secret");
            jail.set_env("DAILY_EPUB_MAIL__SMTP_PASS", "smtp-secret");
            jail.set_env("DAILY_EPUB_VOYAGE__API_KEY", "voyage-key");
            jail.set_env("DAILY_EPUB_VOYAGE__ENABLED", "false");
            jail.set_env("DAILY_EPUB_PROVIDERS__GEMINI__API_KEY", "gemini-key");
            jail.set_env("DAILY_EPUB_LLM__EDITOR", "gemini");

            let c = Config::load(None).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(c.voyage.api_key.as_deref(), Some("voyage-key"));
            assert!(!c.voyage.enabled);
            // The registry is reachable through the same double-underscore path.
            assert_eq!(c.providers["gemini"].api_key.as_deref(), Some("gemini-key"));
            assert!(c.providers["deepseek"].api_key.is_none());
            assert_eq!(c.llm.editor, "gemini");
            assert_eq!(
                c.editor_provider()
                    .map(|(name, p)| (name, p.model.as_str())),
                Some(("gemini", "gemini-3.8-flash"))
            );
            // from file
            assert_eq!(c.lookback_hours, 30);
            assert!(!c.world_briefing);
            assert_eq!(c.miniflux.base_url, "http://127.0.0.1:9999");
            assert_eq!(c.curation.sections, ["Top Stories", "Niche Corner"]);
            assert_eq!(c.xtc.format, XtcFormat::Xtc);
            assert_eq!(c.xtc.args.len(), 2);
            // from env
            assert_eq!(c.miniflux.api_key.as_deref(), Some("secret-token"));
            assert_eq!(c.target_article_count, 12);
            assert_eq!(c.server.hmac_secret.as_deref(), Some("hunter2"));
            assert_eq!(c.bookorbit.opds_pass.as_deref(), Some("orbit-secret"));
            assert_eq!(c.mail.smtp_pass.as_deref(), Some("smtp-secret"));
            // untouched default
            assert_eq!(c.retention_days, 21);
            assert_eq!(c.timezone, "America/New_York");
            Ok(())
        });
    }

    #[test]
    fn bookorbit_activation_and_url_accessors() {
        let mut bookorbit = BookorbitConfig {
            enabled: true,
            public_url: "https://books.example///".into(),
            api_url: "http://127.0.0.1:3498/".into(),
            opds_user: Some("reader".into()),
            opds_pass: Some("secret".into()),
        };
        assert!(bookorbit.is_active());
        assert_eq!(bookorbit.public_url(), "https://books.example");
        assert_eq!(bookorbit.api_url(), "http://127.0.0.1:3498");

        bookorbit.opds_pass = Some("  ".into());
        assert!(!bookorbit.is_active());
    }

    #[test]
    fn mail_defaults_activation_and_validation() {
        let mut mail = MailConfig::default();
        assert!(!mail.enabled);
        assert_eq!(mail.smtp_port, 587);
        assert!(mail.smtp_starttls);
        assert!(!mail.is_active());

        mail.enabled = true;
        assert!(
            Config {
                mail: mail.clone(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );

        mail.smtp_host = "email-smtp.us-east-1.amazonaws.com".into();
        assert!(
            Config {
                mail: mail.clone(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );

        mail.from = "The Daily EPUB <daily@example.com>".into();
        assert!(
            Config {
                mail: mail.clone(),
                ..Config::default()
            }
            .validate()
            .is_ok()
        );
        assert!(!mail.is_active());

        mail.smtp_user = Some("smtp-user".into());
        mail.smtp_pass = Some("smtp-pass".into());
        assert!(mail.is_active());
        mail.smtp_pass = Some("  ".into());
        assert!(!mail.is_active());
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        let _env = env_guard();
        assert!(matches!(
            Config::load(Some(Path::new("/nonexistent/daily-epub.toml"))),
            Err(ConfigError::Missing(_))
        ));
    }

    /// `publish.bookorbit_dir` was renamed to `publish.epub_dir`. A config still
    /// using the old key must fail loudly and name both — silently falling back
    /// to the default would publish the issue into the wrong directory, where
    /// the OPDS feed would then find nothing.
    #[test]
    fn the_renamed_publish_key_fails_loudly() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[publish]\nbookorbit_dir = \"/srv/books\"\nxtc_dir = \"/srv/xtc\"\n",
        )
        .unwrap();

        let err = Config::load(Some(&path)).expect_err("the stale key must be rejected");
        let message = err.to_string();
        assert!(message.contains("bookorbit_dir"), "{message}");
        assert!(message.contains("epub_dir"), "{message}");
    }

    #[test]
    fn removed_prefilter_keep_fails_loudly() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "prefilter_keep = 120\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("the stale key must be rejected");
        let message = error.to_string();
        assert!(message.contains("prefilter_keep"), "{message}");
        assert!(message.contains("curation.ranking.deep_keep"), "{message}");
    }

    #[test]
    fn removed_score_batch_size_names_deep_batch_size() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[deepseek]\nscore_batch_size = 12\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("stale key must fail");
        assert!(error.to_string().contains("deep_batch_size"), "{error}");
    }

    /// The pre-registry `[deepseek]` / `[anthropic]` tables and the role keys
    /// that lived in `[deepseek]` fail loudly, naming their new homes.
    #[test]
    fn stale_provider_tables_and_role_keys_fail_loudly() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        for (body, needles) in [
            (
                "[deepseek]\nmodel = \"deepseek-v4-flash\"\n",
                vec![
                    "[providers.deepseek]",
                    "[llm]",
                    "DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY",
                ],
            ),
            (
                "[anthropic]\nenabled = true\n",
                vec!["[providers.anthropic]", "[llm]"],
            ),
            (
                "[providers.deepseek]\ntriage_batch_size = 25\n",
                vec!["triage_batch_size", "[llm]"],
            ),
            ("editorial_temperature = 0.8\n", vec!["[llm]"]),
            (
                "max_daily_usd = 2.0\n",
                vec!["providers.<name>.max_daily_usd"],
            ),
        ] {
            let path = dir.path().join("config.toml");
            std::fs::write(&path, body).unwrap();
            let error = Config::load(Some(&path)).expect_err(body);
            let message = error.to_string();
            for needle in needles {
                assert!(message.contains(needle), "{body}: {message}");
            }
        }
        // The same keys inside `[llm]`, and a provider's own ceiling, are fine.
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[llm]\ntriage_batch_size = 10\n\n[providers.deepseek]\nmax_daily_usd = 1.5\n",
        )
        .unwrap();
        let c = Config::load(Some(&path)).expect("valid registry config");
        assert_eq!(c.llm.triage_batch_size, 10);
        assert_eq!(c.providers["deepseek"].max_daily_usd, 1.5);
        assert!(stale_toml_error("# [deepseek] in a comment\n").is_none());
    }

    #[test]
    fn stale_env_vars_name_their_replacement() {
        let error =
            stale_env_error(["DAILY_EPUB_DEEPSEEK__API_KEY".to_string()]).expect("stale key var");
        assert!(
            error.contains("DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY"),
            "{error}"
        );
        let error =
            stale_env_error(["DAILY_EPUB_ANTHROPIC__API_KEY".to_string()]).expect("stale key var");
        assert!(
            error.contains("DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY"),
            "{error}"
        );
        let error = stale_env_error(["DAILY_EPUB_DEEPSEEK__SCORE_BATCH_SIZE".to_string()])
            .expect("stale batch var");
        assert!(error.contains("DAILY_EPUB_LLM__DEEP_BATCH_SIZE"), "{error}");
        let error = stale_env_error(["DAILY_EPUB_DEEPSEEK__TRIAGE_BATCH_SIZE".to_string()])
            .expect("stale role var");
        assert!(
            error.contains("DAILY_EPUB_LLM__TRIAGE_BATCH_SIZE"),
            "{error}"
        );
        assert!(
            stale_env_error([
                "DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY".to_string(),
                "DAILY_EPUB_VOYAGE__API_KEY".to_string(),
                "PATH".to_string(),
            ])
            .is_none()
        );
    }

    #[test]
    fn registry_validation_rejects_bad_roles_kinds_and_efforts() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "[providers.mistral]\nkind = \"mistral\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("unknown kind");
        assert!(error.to_string().contains("mistral"), "{error}");

        std::fs::write(&path, "[llm]\neditor = \"gemini2\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("undefined provider");
        assert!(error.to_string().contains("providers.gemini2"), "{error}");

        std::fs::write(&path, "[llm]\nbulk = \"nope\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("undefined bulk provider");
        assert!(error.to_string().contains("llm.bulk"), "{error}");

        std::fs::write(&path, "[llm]\neditor = \"\"\n").unwrap();
        let c = Config::load(Some(&path)).expect("no editor is allowed");
        assert!(c.editor_provider().is_none());
        assert_eq!(c.llm.roles(), vec![("bulk", "deepseek")]);
        assert_eq!(c.referenced_providers().len(), 1);

        std::fs::write(&path, "[llm]\nbulk = \"\"\neditor = \"\"\n").unwrap();
        let c = Config::load(Some(&path)).expect("no providers at all is allowed");
        assert!(c.bulk_provider().is_none());
        assert!(c.referenced_providers().is_empty());

        std::fs::write(
            &path,
            "[llm]\nbulk = \"anthropic\"\neditor = \"anthropic\"\n",
        )
        .unwrap();
        let c = Config::load(Some(&path)).expect("one provider for both roles");
        assert_eq!(c.referenced_providers().len(), 1);

        std::fs::write(&path, "[providers.anthropic]\neffort = \"turbo\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("anthropic effort is an enum");
        assert!(error.to_string().contains("effort"), "{error}");
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            std::fs::write(
                &path,
                format!("[providers.anthropic]\neffort = \"{effort}\"\n"),
            )
            .unwrap();
            assert!(Config::load(Some(&path)).is_ok(), "{effort}");
        }
        std::fs::write(&path, "[providers.gemini]\neffort = \"minimal\"\n").unwrap();
        assert!(
            Config::load(Some(&path)).is_ok(),
            "openai effort is free-form"
        );

        std::fs::write(&path, "[providers.gemini]\nmodel = \"\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("empty model");
        assert!(
            error.to_string().contains("providers.gemini.model"),
            "{error}"
        );
        std::fs::write(&path, "[providers.gemini]\nprice_output_per_mtok = -1\n").unwrap();
        assert!(Config::load(Some(&path)).is_err(), "negative price");
        std::fs::write(&path, "[providers.gemini]\nmax_concurrent_requests = 0\n").unwrap();
        assert!(Config::load(Some(&path)).is_err(), "zero concurrency");
        std::fs::write(&path, "[providers.gemini]\nnot_a_key = 1\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("unknown provider key");
        assert!(error.to_string().contains("not_a_key"), "{error}");
        std::fs::write(&path, "[providers.Gemini]\nmodel = \"x\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("upper-case name");
        assert!(error.to_string().contains("lowercase"), "{error}");
        std::fs::write(&path, "[providers.local]\nmodel = \"llama\"\n").unwrap();
        let error = Config::load(Some(&path)).expect_err("empty base_url");
        assert!(
            error.to_string().contains("providers.local.base_url"),
            "{error}"
        );

        // A brand-new provider only needs the endpoint and model; it is
        // reachable as soon as a role names it.
        std::fs::write(
            &path,
            "[llm]\neditor = \"local\"\n\n[providers.local]\nbase_url = \"http://127.0.0.1:11434/v1\"\nmodel = \"llama\"\n",
        )
        .unwrap();
        let c = Config::load(Some(&path)).expect("new provider");
        let (name, local) = c.editor_provider().expect("editor");
        assert_eq!(name, "local");
        assert_eq!(local.kind, ProviderKind::OpenAi);
        assert_eq!(local.max_daily_usd, 0.0, "no ceiling unless configured");
        assert_eq!(
            ProviderConfig::api_key_env_var("local"),
            "DAILY_EPUB_PROVIDERS__LOCAL__API_KEY"
        );
    }

    #[test]
    fn check_report_lists_every_fact_and_flags_missing_keys() {
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config {
            profile_path: dir.path().join("profile.md"),
            database_path: dir.path().join("missing.db"),
            ..Config::default()
        };
        std::fs::write(&c.profile_path, "# profile\n").unwrap();
        c.providers.get_mut("deepseek").unwrap().api_key = Some("sk-secret".into());
        let lines = c.check_report(Some(Path::new("/etc/daily-epub/config.toml")));
        let text = lines.join("\n");
        assert!(
            !text.contains("sk-secret"),
            "keys are never printed:\n{text}"
        );
        for needle in [
            "config: /etc/daily-epub/config.toml",
            "! database_path: ",
            "(MISSING)",
            "profile_path: ",
            "(exists)",
            "llm.bulk: deepseek · openai · deepseek-v4-flash · effort - · max_daily_usd $2.00 · key present",
            "! llm.editor: anthropic · anthropic · claude-opus-5 · effort high · max_daily_usd $3.00 · key MISSING (set DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY)",
            "providers.gemini: unreferenced · openai · gemini-3.8-flash · key absent",
            "! voyage: voyage-4-lite · enabled · max_daily_usd $0.50 · key MISSING (set DAILY_EPUB_VOYAGE__API_KEY)",
            "editorial.summary_model: editor",
            "publish.epub_dir: ",
            "publish.xtc_dir: ",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(lines.iter().filter(|line| line.starts_with("! ")).count() >= 3);

        c.llm.editor.clear();
        let text = c.check_report(None).join("\n");
        assert!(text.contains("config: built-in defaults"));
        assert!(text.contains("llm.editor: none"), "{text}");
        assert!(text.contains("providers.anthropic: unreferenced"), "{text}");
    }

    #[test]
    fn shipped_example_config_parses() {
        let _env = env_guard();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let c = Config::load(Some(&example)).expect("config.example.toml must parse");
        assert_eq!(c.xtc.command, "node");
        assert_eq!(c.xtc.format, XtcFormat::Xtch);
        assert_eq!(c.server.bind, "127.0.0.1:3499");
        assert_eq!(c.llm.bulk, "deepseek");
        assert_eq!(c.llm.editor, "anthropic");
        assert_eq!(c.llm.triage_batch_size, 25);
        assert_eq!(
            c.providers["deepseek"].base_url,
            "https://api.deepseek.com/v1"
        );
        assert_eq!(c.providers["deepseek"].max_concurrent_requests, 4);
        assert_eq!(c.providers["anthropic"].model, "claude-opus-5");
        assert_eq!(c.providers["anthropic"].effort.as_deref(), Some("high"));
        assert_eq!(c.providers["anthropic"].max_daily_usd, 3.0);
        assert_eq!(c.providers["gemini"].effort.as_deref(), Some("high"));
        assert!(
            c.providers.values().all(|p| p.api_key.is_none()),
            "keys never live in the file"
        );
        assert_eq!(c.curation.max_article_count, 28);
        assert_eq!(c.editorial.summary_model, SummaryModel::Editor);
        assert_eq!(c.editorial.summary_input_tokens, 3000);
    }

    /// `config.example.toml` documents the plan's numbers (§19), which are also
    /// `Config::default()`: every documented key in these sections must exist
    /// on the struct with the default value, and every struct field (except
    /// the env-only `api_key`) must be documented in the file.
    #[test]
    fn shipped_example_config_matches_the_defaults_key_for_key() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let documented: serde_json::Value = Figment::from(Toml::file(&example))
            .extract()
            .expect("config.example.toml must parse as a table");
        let defaults = serde_json::to_value(Config::default()).expect("defaults serialize");

        fn compare(path: &str, documented: &serde_json::Value, default: &serde_json::Value) {
            let (Some(documented), Some(default)) = (documented.as_object(), default.as_object())
            else {
                // TOML `60` and the f64 default `60.0` are the same setting, and
                // an f32 field (`editorial_temperature`) widens inexactly.
                match (documented.as_f64(), default.as_f64()) {
                    (Some(doc), Some(def)) => assert!(
                        (doc - def).abs() <= 1e-6 * def.abs().max(1.0),
                        "{path}: documented {doc} vs default {def}"
                    ),
                    _ => assert_eq!(documented, default, "{path}"),
                }
                return;
            };
            for (key, value) in default {
                if key == "api_key" {
                    assert!(
                        !documented.contains_key(key),
                        "{path}.{key} must stay out of the TOML (env only)"
                    );
                    continue;
                }
                // TOML has no null: an unset `Option` (a provider without an
                // `effort`) is documented by its absence.
                if value.is_null() && !documented.contains_key(key) {
                    continue;
                }
                let doc = documented
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}.{key} is missing from config.example.toml"));
                compare(&format!("{path}.{key}"), doc, value);
            }
            for key in documented.keys() {
                assert!(
                    default.contains_key(key),
                    "{path}.{key} is documented but not a config field"
                );
            }
        }

        for (key, default) in defaults.as_object().expect("config is a table") {
            let section = match key.as_str() {
                "curation" | "llm" | "providers" | "voyage" | "editorial" => key,
                "target_article_count" | "profile_path" | "interests_opml" => key,
                _ => continue,
            };
            let documented = documented
                .get(section)
                .unwrap_or_else(|| panic!("{section} is missing from config.example.toml"));
            compare(section, documented, default);
        }
    }

    #[test]
    fn provider_validation_rejects_nonsense() {
        let mut c = Config::default();
        c.curation.max_article_count = c.target_article_count - 1;
        assert!(c.validate().is_err(), "max_article_count below the target");
        let mut c = Config::default();
        c.providers.get_mut("anthropic").unwrap().effort = Some("turbo".into());
        assert!(c.validate().is_err(), "unknown effort");
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let mut c = Config::default();
            c.providers.get_mut("anthropic").unwrap().effort = Some(effort.into());
            assert!(c.validate().is_ok(), "{effort} is a valid effort");
        }
        let mut c = Config::default();
        c.providers
            .get_mut("deepseek")
            .unwrap()
            .max_concurrent_requests = 0;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.providers.get_mut("anthropic").unwrap().max_daily_usd = -1.0;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.llm.deep_batch_size = 0;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.llm.triage_batch_size = 0;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.editorial.summary_input_tokens = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn voyage_and_ranking_defaults_and_validation() {
        let _env = env_guard();
        let cfg = Config::default();
        assert!(cfg.voyage.enabled);
        assert_eq!(cfg.voyage.base_url, "https://api.voyageai.com/v1");
        assert_eq!(cfg.voyage.model, "voyage-4-lite");
        assert_eq!(cfg.voyage.output_dimension, 512);
        assert_eq!(cfg.voyage.batch_size, 32);
        assert_eq!(cfg.voyage.max_concurrent_requests, 4);
        assert_eq!(cfg.voyage.max_input_chars, 60_000);
        assert_eq!(cfg.voyage.max_daily_usd, 0.50);
        let ranking = &cfg.curation.ranking;
        assert_eq!(
            (
                ranking.triage_max,
                ranking.deep_keep,
                ranking.shortlist_keep
            ),
            (800, 120, 60)
        );
        assert_eq!((ranking.knn_floor, ranking.knn_full), (8, 25));
        assert_eq!((ranking.feed_floor, ranking.feed_full), (15, 40));
        assert_eq!(ranking.rating_half_life_days, 60.0);
        assert_eq!(ranking.negative_coefficient, 0.75);
        assert_eq!(ranking.weights.preliminary.interest, 0.35);
        assert_eq!(ranking.weights.utility.quality, 0.40);
        assert_eq!(ranking.diversity.per_cluster_cap, 2);
        assert_eq!(ranking.embedding_retention_days, 120);
        assert_eq!(ranking.telemetry_retention_days, 180);
        cfg.validate().unwrap();

        let mut bad = Config::default();
        bad.voyage.output_dimension = 300;
        assert!(bad.validate().is_err(), "dimension must be a Voyage size");
        let mut bad = Config::default();
        bad.voyage.batch_size = 0;
        assert!(bad.validate().is_err());
        let mut bad = Config::default();
        bad.curation.ranking.weights.preliminary.knn = -0.1;
        assert!(bad.validate().is_err(), "weights are non-negative");
        let mut bad = Config::default();
        bad.curation.ranking.knn_full = bad.curation.ranking.knn_floor;
        assert!(bad.validate().is_err(), "*_full must exceed *_floor");
        let mut bad = Config::default();
        bad.curation.ranking.shortlist_keep = bad.curation.ranking.deep_keep + 1;
        assert!(bad.validate().is_err(), "deep_keep >= shortlist_keep");
        let mut bad = Config::default();
        bad.curation.ranking.shortlist_keep = bad.target_article_count - 1;
        assert!(bad.validate().is_err(), "shortlist_keep >= target");
        let mut bad = Config::default();
        bad.curation.ranking.diversity.cluster_threshold = 1.5;
        assert!(bad.validate().is_err());
        let mut bad = Config::default();
        bad.curation.ranking.diversity.per_cluster_cap = 0;
        assert!(bad.validate().is_err());

        // Unknown keys inside a known section fail loudly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[voyage]\nenabled = true\nnot_a_key = 1\n").unwrap();
        let err = Config::load(Some(&path)).expect_err("unknown voyage key must be rejected");
        assert!(err.to_string().contains("not_a_key"), "{err}");
    }

    #[test]
    fn validation_rejects_nonsense() {
        let mut too_small = Config::default();
        too_small.curation.ranking.deep_keep = 5;
        assert!(too_small.validate().is_err());
        assert!(
            Config {
                timezone: "Mars/Olympus_Mons".into(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                lookback_hours: 0,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }
}
