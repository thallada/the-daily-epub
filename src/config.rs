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
    /// Days of published files kept in the publish dirs (§3.11).
    pub retention_days: u32,
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

    pub miniflux: MinifluxConfig,
    pub deepseek: DeepseekConfig,
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
            max_daily_usd: 2.0,
            world_briefing: true,
            database_path: PathBuf::from("/var/lib/daily-epub/daily-epub.db"),
            out_dir: PathBuf::from("/var/lib/daily-epub/out"),
            interests_opml: PathBuf::from("data/scour-interests.opml"),
            miniflux: MinifluxConfig::default(),
            deepseek: DeepseekConfig::default(),
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
        }
    }
}

/// `[publish]` — where finished artifacts land (§3.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PublishConfig {
    /// BookOrbit "The Daily EPUB" library watched folder.
    pub bookorbit_dir: PathBuf,
    /// Directory served at `/files/xtc/`.
    pub xtc_dir: PathBuf,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            bookorbit_dir: PathBuf::from("/srv/bookorbit/libraries/daily-epub"),
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
    /// Optional Basic auth for `/opds/xtc.xml` and `/files/xtc/`.
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

            let c = Config::load(None).map_err(|e| figment::Error::from(e.to_string()))?;
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
