//! Typed configuration (spec §3.14).
//!
//! Load order, later wins: built-in defaults ← `config.toml` (path from `--config`,
//! else `./config.toml` if present) ← `DAILY_EPUB_*` environment variables, where
//! nesting is expressed with a double underscore (`DAILY_EPUB_MINIFLUX__API_KEY`).

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
    /// How many articles the lineup should contain (§3.6 stage B).
    pub target_article_count: usize,
    /// How many articles survive the heuristic pre-filter (§3.5).
    pub prefilter_keep: usize,
    /// Days of published EPUBs kept in `publish.epub_dir` (§3.11).
    pub retention_days: u32,
    /// How many XTC issues to keep in `publish.xtc_dir` (§3.11).
    ///
    /// Counted, not dated, because an XTCH issue is ~80–100 MB of pre-rendered
    /// page bitmaps: the constraint is disk, not age.
    pub xtc_retention_count: u32,
    /// Hard cost ceiling per run (§3.6 guardrail).
    pub max_daily_usd: f64,
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
    pub deepseek: DeepseekConfig,
    pub voyage: VoyageConfig,
    pub curation: CurationConfig,
    pub publish: PublishConfig,
    pub xtc: XtcConfig,
    pub server: ServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timezone: "America/New_York".into(),
            lookback_hours: 26,
            target_article_count: 20,
            prefilter_keep: 120,
            retention_days: 21,
            xtc_retention_count: 5,
            max_daily_usd: 2.0,
            world_briefing: true,
            database_path: PathBuf::from("/var/lib/daily-epub/daily-epub.db"),
            out_dir: PathBuf::from("/var/lib/daily-epub/out"),
            interests_opml: PathBuf::from("data/scour-interests.opml"),
            profile_path: PathBuf::from("data/profile.md"),
            miniflux: MinifluxConfig::default(),
            deepseek: DeepseekConfig::default(),
            voyage: VoyageConfig::default(),
            curation: CurationConfig::default(),
            publish: PublishConfig::default(),
            xtc: XtcConfig::default(),
            server: ServerConfig::default(),
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

/// `[deepseek]` — LLM endpoint, model and pricing (§3.6, notes "verified facts").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeepseekConfig {
    pub base_url: String,
    pub model: String,
    /// Supply via `DAILY_EPUB_DEEPSEEK__API_KEY`.
    pub api_key: Option<String>,
    /// Articles per stage-A scoring request (§3.6).
    pub score_batch_size: usize,
    pub score_temperature: f32,
    pub editorial_temperature: f32,
    /// USD per 1M cache-miss input tokens.
    pub price_input_per_mtok: f64,
    /// USD per 1M prefix-cache-hit input tokens.
    pub price_cached_input_per_mtok: f64,
    /// USD per 1M output tokens.
    pub price_output_per_mtok: f64,
}

impl Default for DeepseekConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
            api_key: None,
            score_batch_size: 12,
            score_temperature: 0.3,
            editorial_temperature: 0.8,
            price_input_per_mtok: 0.14,
            price_cached_input_per_mtok: 0.0028,
            price_output_per_mtok: 0.28,
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

/// `[curation]` — pre-filter and section palette (§3.5, §3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CurationConfig {
    /// Miniflux feed ids or site URLs that can never be dropped (§3.5).
    pub always_include_feeds: Vec<String>,
    /// Hosts excluded outright (§3.5).
    pub blocked_domains: Vec<String>,
    /// Extra paywalled hosts, merged with [`crate::extract::DEFAULT_PAYWALL_DOMAINS`]
    /// by the extraction stage's `excerpt_only` heuristic (§3.3).
    pub paywall_domains: Vec<String>,
    /// The only section names the LLM may use (§3.6 stage B).
    pub sections: Vec<String>,
    pub feedback: FeedbackConfig,
    pub ranking: RankingConfig,
}

impl Default for CurationConfig {
    fn default() -> Self {
        Self {
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
/// personalized ranker (plan §19). Steps 4–5 consume most of these; step 3
/// uses the learned-signal gates, the preliminary weights and the retention
/// windows.
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
    pub verdicts_in_prompt: usize,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            loved_value: 1.0,
            good_value: 0.35,
            not_for_me_value: -1.0,
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3499".into(),
            public_url: "https://daily.hallada.net".into(),
            hmac_secret: None,
            basic_auth_user: None,
            basic_auth_pass: None,
        }
    }
}

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

    /// Load config for the CLI: explicit `--config` path, else `./config.toml`
    /// when it exists, then `DAILY_EPUB_*` env overrides (§3.14).
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let (path, require) = match explicit {
            Some(p) => (Some(p.to_path_buf()), true),
            None => (Some(PathBuf::from(DEFAULT_CONFIG_FILE)), false),
        };
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

    /// Cheap sanity checks so misconfiguration fails at startup, not mid-run.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.lookback_hours == 0 {
            return Err(ConfigError::Invalid("lookback_hours must be > 0".into()));
        }
        if self.target_article_count == 0 {
            return Err(ConfigError::Invalid(
                "target_article_count must be > 0".into(),
            ));
        }
        if self.prefilter_keep < self.target_article_count {
            return Err(ConfigError::Invalid(
                "prefilter_keep must be >= target_article_count".into(),
            ));
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
        if self.deepseek.score_batch_size == 0
            || self.voyage.batch_size == 0
            || self.voyage.max_concurrent_requests == 0
        {
            return Err(ConfigError::Invalid(
                "provider batch sizes must be >= 1".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.timezone, "America/New_York");
        assert_eq!(c.lookback_hours, 26);
        assert_eq!(c.target_article_count, 20);
        assert_eq!(c.prefilter_keep, 120);
        assert_eq!(c.retention_days, 21);
        assert_eq!(c.max_daily_usd, 2.0);
        assert!(c.world_briefing);
        assert_eq!(c.deepseek.model, "deepseek-v4-flash");
        assert_eq!(c.profile_path, PathBuf::from("data/profile.md"));
        assert_eq!(c.curation.feedback.good_value, 0.35);
        assert_eq!(c.curation.feedback.verdicts_in_prompt, 60);
        assert_eq!(c.xtc.format, XtcFormat::Xtch);
        assert_eq!(c.curation.sections.len(), 8);
        c.validate().unwrap();
    }

    #[test]
    // `Jail::expect_with` dictates the closure's `figment::Error` return type.
    #[allow(clippy::result_large_err)]
    fn toml_then_env_layering() {
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
            jail.set_env("DAILY_EPUB_VOYAGE__API_KEY", "voyage-key");
            jail.set_env("DAILY_EPUB_VOYAGE__ENABLED", "false");

            let c = Config::load(None).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(c.voyage.api_key.as_deref(), Some("voyage-key"));
            assert!(!c.voyage.enabled);
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
            // untouched default
            assert_eq!(c.retention_days, 21);
            assert_eq!(c.timezone, "America/New_York");
            Ok(())
        });
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
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
    fn shipped_example_config_parses() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let c = Config::load(Some(&example)).expect("config.example.toml must parse");
        assert_eq!(c.xtc.command, "node");
        assert_eq!(c.xtc.format, XtcFormat::Xtch);
        assert_eq!(c.server.bind, "127.0.0.1:3499");
        assert_eq!(c.deepseek.base_url, "https://api.deepseek.com/v1");
    }

    #[test]
    fn voyage_and_ranking_defaults_and_validation() {
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
        assert!(
            Config {
                prefilter_keep: 5,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
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
