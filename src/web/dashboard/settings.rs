//! Dashboard: settings pages (web dashboard plan §13).
//!
//! The schema is *derived*: [`schema`] walks `toml::Value::try_from(Config::default())`
//! and the live config in parallel, so a new key in [`Config`] shows up on the
//! page without UI work. Saving goes through `toml_edit` so comments and key
//! order in `config.toml` survive, and every write is validated by
//! [`Config::load`] on a temp file before it is renamed over the original.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use askama::Template;
use axum::Form;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use serde::Deserialize;
use sqlx::Row;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::config::{
    Config, ENV_PREFIX, ENV_SECRET_ALIAS, ENV_SPLIT, ProviderConfig, ProviderKind,
    default_providers,
};
use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Flash, Html, Page, Pagination, WebError, WebState, format_time, take_flash};

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/settings", get(show).post(save))
        .route(
            "/dashboard/settings/providers",
            axum::routing::post(providers),
        )
        .route("/dashboard/settings/history", get(history))
}

// ---------------------------------------------------------------------------
// Schema (§13.1)
// ---------------------------------------------------------------------------

/// How a leaf renders and how a posted value is typed before it is written.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Bool,
    Integer,
    Float,
    Text,
    /// An array of strings, edited one item per line.
    TextList,
    /// A fixed set of accepted values (an empty string means "absent" for
    /// optional keys such as `providers.*.effort`).
    Enum(Vec<String>),
    /// Never rendered, never written; shown as set / not set.
    Secret,
    Path,
}

/// Where the live value of a key comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    /// Overridden by this environment variable; the field is locked.
    Env(String),
}

#[derive(Debug, Clone)]
pub struct SettingField {
    /// Dotted path, e.g. `curation.ranking.deep_keep`.
    pub path: String,
    /// The enclosing table, e.g. `curation.ranking` (empty at the top level).
    pub group: String,
    /// The last path segment.
    pub key: String,
    pub kind: FieldKind,
    /// Rendered live value (`TextList`: one item per line; `Secret`: never the value).
    pub current: String,
    pub default: String,
    pub source: Source,
    pub help: Option<&'static str>,
    pub restart_required: bool,
}

impl SettingField {
    pub fn input_id(&self) -> String {
        format!("f-{}", self.path.replace('.', "-"))
    }

    pub fn is_secret(&self) -> bool {
        self.kind == FieldKind::Secret
    }

    pub fn is_env(&self) -> bool {
        matches!(self.source, Source::Env(_))
    }

    pub fn env_var(&self) -> String {
        match &self.source {
            Source::Env(name) => name.clone(),
            _ => env_name(&self.path),
        }
    }

    pub fn is_file(&self) -> bool {
        self.source == Source::File
    }

    pub fn is_bool(&self) -> bool {
        self.kind == FieldKind::Bool
    }

    pub fn is_text_list(&self) -> bool {
        self.kind == FieldKind::TextList
    }

    pub fn is_number(&self) -> bool {
        matches!(self.kind, FieldKind::Integer | FieldKind::Float)
    }

    pub fn is_integer(&self) -> bool {
        self.kind == FieldKind::Integer
    }

    pub fn options(&self) -> &[String] {
        match &self.kind {
            FieldKind::Enum(options) => options,
            _ => &[],
        }
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.kind, FieldKind::Enum(_))
    }

    /// Whether the value is set (only meaningful for secrets).
    pub fn is_set(&self) -> bool {
        self.current == "set"
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            FieldKind::Bool => "bool",
            FieldKind::Integer => "integer",
            FieldKind::Float => "float",
            FieldKind::Text => "text",
            FieldKind::TextList => "list",
            FieldKind::Enum(_) => "enum",
            FieldKind::Secret => "secret",
            FieldKind::Path => "path",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingGroup {
    /// The anchor: the dotted path of the table (`general` at the top level).
    pub id: String,
    pub title: String,
    pub fields: Vec<SettingField>,
    /// Set for `providers.<name>` cards.
    pub provider: Option<ProviderCard>,
}

impl SettingGroup {
    pub fn is_weights(&self) -> bool {
        self.id.starts_with("curation.ranking.weights.")
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCard {
    pub name: String,
    /// The `[llm]` roles that name this provider; non-empty ⇒ cannot be removed.
    pub referenced_by: Vec<&'static str>,
    /// Declared by `Config::default()` (the shipped registry): removing its
    /// table from the file would not remove the provider, so it cannot be
    /// removed here.
    pub built_in: bool,
}

impl ProviderCard {
    pub fn roles(&self) -> String {
        self.referenced_by.join(", ")
    }

    pub fn removable(&self) -> bool {
        self.referenced_by.is_empty() && !self.built_in
    }
}

/// Keys whose new value only takes effect after the server restarts (they are
/// read once when the listener, the session layer or the login throttle are
/// built).
pub const RESTART_REQUIRED: &[&str] = &[
    "database_path",
    "server.bind",
    "server.public_url",
    "server.session_days",
    "server.login_attempts",
    "server.login_window_minutes",
    "mail.enabled",
    "mail.smtp_host",
    "mail.smtp_port",
    "mail.smtp_starttls",
    "mail.smtp_user",
    "mail.smtp_pass",
    "mail.from",
    "mail.notify_to",
];

/// Optional keys that `Config::default()` leaves unset (and therefore do not
/// appear when the defaults are serialized), with the kind they take.
const OPTIONAL_KEYS: &[(&str, FieldKind)] = &[
    ("miniflux.api_key", FieldKind::Secret),
    ("providers.*.api_key", FieldKind::Secret),
    ("providers.*.effort", FieldKind::Text),
    ("voyage.api_key", FieldKind::Secret),
    ("server.hmac_secret", FieldKind::Secret),
    ("server.basic_auth_user", FieldKind::Text),
    ("server.basic_auth_pass", FieldKind::Secret),
    ("bookorbit.opds_user", FieldKind::Text),
    ("bookorbit.opds_pass", FieldKind::Secret),
    ("mail.smtp_user", FieldKind::Text),
    ("mail.smtp_pass", FieldKind::Secret),
    ("mail.notify_to", FieldKind::Text),
    ("xtc.settings", FieldKind::Path),
];

const SECRET_SUFFIXES: &[&str] = &[
    "api_key",
    "hmac_secret",
    "basic_auth_pass",
    "opds_pass",
    "smtp_pass",
];

const PATH_KEYS: &[&str] = &[
    "database_path",
    "out_dir",
    "profile_path",
    "interests_opml",
    "publish.epub_dir",
    "publish.xtc_dir",
    "xtc.settings",
];

const PROVIDER_KINDS: &[&str] = &["openai", "anthropic"];
const ANTHROPIC_EFFORTS: &[&str] = &["", "low", "medium", "high", "xhigh", "max"];
const SUMMARY_MODELS: &[&str] = &["editor", "bulk"];
const XTC_FORMATS: &[&str] = &["xtc", "xtch"];

/// The rendering order of the group cards (§13.1 item 5); anything the walk
/// finds that is not listed here renders after them, in walk order.
const GROUP_ORDER: &[&str] = &[
    "",
    "llm",
    "providers.*",
    "voyage",
    "curation",
    "curation.feedback",
    "curation.ranking",
    "curation.ranking.quotas",
    "curation.ranking.weights.preliminary",
    "curation.ranking.weights.utility",
    "curation.ranking.diversity",
    "editorial",
    "publish",
    "xtc",
    "server",
    "miniflux",
    "bookorbit",
    "mail",
    "discovery",
];

/// Help text per key, seeded from the README configuration table and the
/// comments in `config.example.toml`. `providers.*` entries cover every
/// provider table.
#[rustfmt::skip]
pub const SETTINGS_HELP: &[(&str, &str)] = &[
    ("timezone", "IANA time zone for day boundaries and --date interpretation."),
    ("lookback_hours", "Size of the ingest window ending at the issue day's end (clamped to now)."),
    ("target_article_count", "Soft target the editor aims for. There is no minimum: a nine-pick issue is published as nine."),
    ("retention_days", "EPUBs older than this are deleted from publish.epub_dir. SQLite history is kept forever."),
    ("xtc_retention_count", "How many XTC issues to keep in publish.xtc_dir. Counted, not dated: each .xtch is ~80-100 MB, so the binding constraint is disk, not age."),
    ("world_briefing", "Include the Wikipedia Current Events section."),
    ("database_path", "SQLite file; parent directories are created on demand."),
    ("out_dir", "Where generate writes artifacts before publishing (overridden by --out)."),
    ("profile_path", "Hand-maintained reader profile, loaded every run."),
    ("interests_opml", "Scour interests OPML merged with the profile interests."),
    ("miniflux.base_url", "Miniflux root (no /v1)."),
    ("miniflux.api_key", "X-Auth-Token for Miniflux. Required; environment only."),
    ("miniflux.page_limit", "Entries per page for GET /v1/entries; Miniflux caps this at 250."),
    ("llm.bulk", "The [providers.*] name that runs triage, deep assessment and every fallback. Empty means no bulk provider (those stages are skipped)."),
    ("llm.editor", "The provider that assembles the lineup, writes the summaries and The Brief and rebuilds the profile. Empty means everything runs on bulk. Naming the same provider as bulk shares one client and one ceiling."),
    ("llm.triage_batch_size", "Articles per first-pass triage request."),
    ("llm.deep_batch_size", "Articles per close-reading assessment request."),
    ("llm.score_temperature", "Scoring temperature, sent only to openai-kind providers."),
    ("llm.editorial_temperature", "Summaries and The Brief on an openai-kind provider."),
    ("providers.*.kind", "openai (chat completions at {base_url}/chat/completions, bearer key) or anthropic (the Messages API with output_config.effort and a cached system block)."),
    ("providers.*.base_url", "Endpoint root. DeepSeek https://api.deepseek.com/v1; Anthropic https://api.anthropic.com; Gemini https://generativelanguage.googleapis.com/v1beta/openai."),
    ("providers.*.model", "Model name sent with every request."),
    ("providers.*.api_key", "Environment only (DAILY_EPUB_PROVIDERS__<NAME>__API_KEY). Absent means that role is unavailable and degrades (bulk to heuristic curation, editor to bulk)."),
    ("providers.*.effort", "anthropic: low, medium, high, xhigh or max (output_config.effort). openai: passed through as reasoning_effort (Gemini takes minimal-high); leave empty for models without one (DeepSeek)."),
    ("providers.*.max_daily_usd", "That provider's ceiling per UTC day of the run's start, not per run. Tripping it skips the provider's remaining calls; the paper still publishes. 0 disables the guard."),
    ("providers.*.max_concurrent_requests", "Triage and deep-assessment batches in flight on the bulk provider; summaries in flight on the summary provider."),
    ("providers.*.price_input_per_mtok", "USD per 1M cache-miss input tokens (cost guardrail arithmetic only)."),
    ("providers.*.price_cache_read_per_mtok", "USD per 1M cache-hit input tokens."),
    ("providers.*.price_cache_write_per_mtok", "USD per 1M tokens written to the prompt cache (implicit caches charge nothing)."),
    ("providers.*.price_output_per_mtok", "USD per 1M output tokens, thinking tokens included where the provider bills them as output."),
    ("voyage.enabled", "Embed articles and interests with Voyage AI. Off means cached vectors only; the learned signals are simply absent, never a penalty."),
    ("voyage.base_url", "POST {base_url}/embeddings."),
    ("voyage.model", "Embedding model; changing it invalidates the cache."),
    ("voyage.api_key", "Environment only (DAILY_EPUB_VOYAGE__API_KEY). Absent means cached vectors only."),
    ("voyage.output_dimension", "One of 256, 512, 1024, 2048."),
    ("voyage.batch_size", "Texts per request."),
    ("voyage.max_concurrent_requests", "Requests in flight."),
    ("voyage.max_input_chars", "Per-article cut, on a char boundary."),
    ("voyage.max_daily_usd", "Runaway guard at $0.02 per 1M tokens."),
    ("curation.max_article_count", "Hard ceiling on issue size; there is no minimum. Must be >= target_article_count."),
    ("curation.recent_rejection_days", "Churn window for recent low triage/deep assessments."),
    ("curation.recent_rejection_floor", "Scores below this floor are excluded during the churn window (except auto-includes)."),
    ("curation.always_include_feeds", "Miniflux feed ids or URL substrings that can never be dropped. One per line."),
    ("curation.blocked_domains", "Hosts excluded outright. One per line."),
    ("curation.paywall_domains", "Extra paywalled hosts, merged with the built-in list (nytimes, wsj, ft, economist, ...). A short body from one of these is marked excerpt-only and penalized. One per line."),
    ("curation.sections", "The only section names the editor may use, one per line. World Briefing is reserved and never offered."),
    ("curation.feedback.loved_value", "Weight for a Loved it verdict."),
    ("curation.feedback.good_value", "Weight for a Good verdict."),
    ("curation.feedback.not_for_me_value", "Weight for a Not for me verdict."),
    ("curation.feedback.verdicts_in_prompt", "Recent explicit verdicts included in the system prompt."),
    ("curation.ranking.triage_max", "Eligible articles the triage LLM reads."),
    ("curation.ranking.deep_keep", "Size of the deep-assessment set. Must be >= shortlist_keep."),
    ("curation.ranking.shortlist_keep", "What the editor sees. Must be >= target_article_count."),
    ("curation.ranking.assessment_reuse_days", "Days a stored deep assessment is reused instead of re-asked."),
    ("curation.ranking.rating_lookback_days", "How far back explicit ratings count."),
    ("curation.ranking.rating_half_life_days", "Ratings decay with this half-life."),
    ("curation.ranking.neighbour_k", "Rated neighbours per side for the knn signal."),
    ("curation.ranking.negative_coefficient", "How strongly Not for me neighbours pull a candidate down."),
    ("curation.ranking.knn_floor", "Rated articles with embeddings before the knn signal starts to count."),
    ("curation.ranking.knn_full", "Rated articles at which the knn signal reaches full weight. Must be > knn_floor."),
    ("curation.ranking.feed_floor", "Attributable ratings before the feed-affinity signal starts to count."),
    ("curation.ranking.feed_full", "Attributable ratings at which feed affinity reaches full weight. Must be > feed_floor."),
    ("curation.ranking.semantic_min_words", "Bodies shorter than this are not embedded."),
    ("curation.ranking.exploration_slots", "Shortlist slots reserved for exploration picks."),
    ("curation.ranking.embedding_retention_days", "features prune: unrated, unpublished vectors older than this are deleted."),
    ("curation.ranking.telemetry_retention_days", "features prune: candidate_runs and article_assessments older than this are deleted."),
    ("curation.ranking.quotas.triage", "Deep-set slots filled by triage score."),
    ("curation.ranking.quotas.interest", "Deep-set slots filled by interest similarity."),
    ("curation.ranking.quotas.knn", "Deep-set slots filled by rated-neighbour preference."),
    ("curation.ranking.weights.preliminary.interest", "Interest similarity in the preliminary blend."),
    ("curation.ranking.weights.preliminary.knn", "Rated-neighbour preference in the preliminary blend."),
    ("curation.ranking.weights.preliminary.heuristic", "Heuristic score in the preliminary blend."),
    ("curation.ranking.weights.preliminary.feed", "Feed affinity in the preliminary blend."),
    ("curation.ranking.weights.preliminary.social", "Social signal in the preliminary blend."),
    ("curation.ranking.weights.utility.quality", "Deep-assessment quality in the utility score."),
    ("curation.ranking.weights.utility.fit", "Deep-assessment fit in the utility score."),
    ("curation.ranking.weights.utility.knn", "Rated-neighbour preference in the utility score."),
    ("curation.ranking.weights.utility.interest", "Interest similarity in the utility score."),
    ("curation.ranking.weights.utility.feed", "Feed affinity in the utility score."),
    ("curation.ranking.weights.utility.triage", "Triage score in the utility score."),
    ("curation.ranking.weights.utility.social", "Social signal in the utility score."),
    ("curation.ranking.weights.utility.heuristic", "Heuristic score in the utility score."),
    ("curation.ranking.diversity.cluster_threshold", "Cosine similarity at which two candidates share a cluster (0-1)."),
    ("curation.ranking.diversity.per_cluster_cap", "Shortlist picks allowed per cluster."),
    ("curation.ranking.diversity.utility_protected", "Top-utility candidates exempt from the cluster cap."),
    ("editorial.summary_model", "Which [llm] role writes the per-article summaries: editor (with per-article bulk fallback) or bulk."),
    ("editorial.summary_input_tokens", "Article text offered to the summary prompt."),
    ("publish.epub_dir", "Both EPUB editions land here by atomic copy, and this is the directory the OPDS feed lists. Point a BookOrbit watched folder at it if you want its UI too."),
    ("publish.xtc_dir", "XTC artifacts. Not listed in the OPDS feed, but downloadable at /files/xtc/<name> for sideloading."),
    ("xtc.enabled", "Off skips the converter entirely."),
    ("xtc.command", "Converter executable (the converter has no global npm bin, so it runs through node)."),
    ("xtc.args", "Prefix arguments, one per line; the code appends <input.epub> -o <output> -f <format> (plus -c <settings>)."),
    ("xtc.format", "xtc (1-bit) or xtch (2-bit grayscale, better images, ~96 KB per rendered page)."),
    ("xtc.settings", "Settings JSON passed as -c. Required in practice: without font.path the converter exits before doing any work. Start from xtc-settings.example.json."),
    ("server.bind", "Listen address."),
    ("server.public_url", "Base URL the rating links inside the EPUB are built from."),
    ("server.hmac_secret", "Environment only (DAILY_EPUB_SERVER__HMAC_SECRET or DAILY_EPUB_SECRET). Without it, rating links are rejected with 403."),
    ("server.basic_auth_user", "Optional Basic auth user for /opds/* and /files/*; signed-in web users may download without it."),
    ("server.basic_auth_pass", "Password for server.basic_auth_user. Environment only."),
    ("server.session_days", "Sliding lifetime for dashboard login sessions."),
    ("server.login_attempts", "Login attempts allowed per IP in one throttle window."),
    ("server.login_window_minutes", "Length of the login throttle window."),
    ("server.jobs_enabled", "Allow the dashboard to start the fixed systemd job catalogue."),
    ("server.journal_lines", "Journal lines shown on a dashboard job page (10-5000)."),
    ("bookorbit.enabled", "Enable the signed-in Read in BookOrbit integration when OPDS credentials are also set."),
    ("bookorbit.public_url", "Base URL opened in the browser for BookOrbit's web reader."),
    ("bookorbit.api_url", "Base URL used by the server for BookOrbit OPDS requests; usually the loopback address."),
    ("bookorbit.opds_user", "Dedicated OPDS user created in BookOrbit Settings → OPDS."),
    ("bookorbit.opds_pass", "Password for bookorbit.opds_user. Environment only."),
    ("mail.enabled", "Enable outbound SMTP when the relay, sender and credentials are configured. Requires a server restart."),
    ("mail.smtp_host", "SMTP relay hostname, such as an AWS SES SMTP endpoint. Requires a server restart."),
    ("mail.smtp_port", "SMTP relay port: usually 587 for STARTTLS or 465 for implicit TLS. Requires a server restart."),
    ("mail.smtp_starttls", "Use STARTTLS when true; false uses implicit TLS. Requires a server restart."),
    ("mail.smtp_user", "SMTP username. Requires a server restart."),
    ("mail.smtp_pass", "SMTP password. Environment only; requires a server restart."),
    ("mail.from", "Sender mailbox as an address or Name <address>. Requires a server restart."),
    ("mail.notify_to", "Recipient for new access-request notifications. Requires a server restart."),
    ("discovery.enabled", "Run the feed discovery stage during generate."),
    ("discovery.max_lookups_per_run", "How many not-yet-checked hosts one run may look up in Miniflux; each host is re-checked at most every 90 days."),
    ("discovery.skip_hosts", "Hosts never looked up, one per line. Matches the host or any subdomain of it."),
];

/// `DAILY_EPUB_` + the path upper-cased with `.` → `__` (§13.1 item 2).
pub fn env_name(path: &str) -> String {
    format!(
        "{ENV_PREFIX}{}",
        path.to_uppercase().replace('.', ENV_SPLIT)
    )
}

fn matches_pattern(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.split('.');
    let mut path = path.split('.');
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            (Some(p), Some(segment)) if p == "*" || p == segment => continue,
            _ => return false,
        }
    }
}

/// The help entry for a key (`providers.<name>.x` matches `providers.*.x`).
pub fn help_for(path: &str) -> Option<&'static str> {
    SETTINGS_HELP
        .iter()
        .find(|(pattern, _)| matches_pattern(pattern, path))
        .map(|(_, help)| *help)
}

fn optional_kind(path: &str) -> Option<&'static FieldKind> {
    OPTIONAL_KEYS
        .iter()
        .find(|(pattern, _)| matches_pattern(pattern, path))
        .map(|(_, kind)| kind)
}

fn is_optional(path: &str) -> bool {
    optional_kind(path).is_some()
}

/// Render a TOML value the way the form shows it (arrays one item per line).
fn render_toml(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => fmt_float(*f),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Array(items) => items.iter().map(render_toml).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

/// At most six decimals, trailing zeros trimmed, always one decimal digit.
pub fn fmt_float(value: f64) -> String {
    let text = format!("{:.6}", round6(value));
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// `has_default`: `Config::default()` holds a value for this key, so an
/// optional key cannot be unset through the file (the default would fill it
/// back in) and the "(none)" option is not offered.
fn kind_for(
    path: &str,
    config: &Config,
    sample: Option<&toml::Value>,
    has_default: bool,
) -> FieldKind {
    let last = path.rsplit('.').next().unwrap_or(path);
    if SECRET_SUFFIXES.contains(&last) {
        return FieldKind::Secret;
    }
    let segments: Vec<&str> = path.split('.').collect();
    if let ["providers", name, key] = segments.as_slice() {
        match *key {
            "kind" => return FieldKind::Enum(strings(PROVIDER_KINDS)),
            "effort" => {
                let anthropic = config
                    .providers
                    .get(*name)
                    .is_some_and(|provider| provider.kind == ProviderKind::Anthropic);
                return if anthropic {
                    let offered = if has_default {
                        &ANTHROPIC_EFFORTS[1..]
                    } else {
                        ANTHROPIC_EFFORTS
                    };
                    FieldKind::Enum(strings(offered))
                } else {
                    FieldKind::Text
                };
            }
            _ => {}
        }
    }
    match path {
        "editorial.summary_model" => return FieldKind::Enum(strings(SUMMARY_MODELS)),
        "xtc.format" => return FieldKind::Enum(strings(XTC_FORMATS)),
        "llm.bulk" => return FieldKind::Enum(config.providers.keys().cloned().collect()),
        "llm.editor" => {
            let mut options = vec![String::new()];
            options.extend(config.providers.keys().cloned());
            return FieldKind::Enum(options);
        }
        _ => {}
    }
    if PATH_KEYS.contains(&path) {
        return FieldKind::Path;
    }
    match sample {
        Some(toml::Value::Boolean(_)) => FieldKind::Bool,
        Some(toml::Value::Integer(_)) => FieldKind::Integer,
        Some(toml::Value::Float(_)) => FieldKind::Float,
        Some(toml::Value::Array(_)) => FieldKind::TextList,
        Some(_) => FieldKind::Text,
        None => optional_kind(path).cloned().unwrap_or(FieldKind::Text),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// Look a dotted path up in a parsed document.
fn file_item<'a>(doc: &'a DocumentMut, path: &str) -> Option<&'a Item> {
    let mut item = doc.as_item();
    for segment in path.split('.') {
        item = item.get(segment)?;
    }
    Some(item)
}

fn config_table(config: &Config) -> toml::Table {
    match toml::Value::try_from(config) {
        Ok(toml::Value::Table(table)) => table,
        Ok(_) => toml::Table::new(),
        Err(error) => {
            tracing::error!(%error, "serializing the configuration for the settings page failed");
            toml::Table::new()
        }
    }
}

/// Derive the settings schema (§13.1) for the live `config`, with `file` the
/// parsed `config.toml` (for the `File` source), reading the process
/// environment for `Env` sources.
pub fn schema(config: &Config, file: Option<&DocumentMut>) -> Vec<SettingGroup> {
    schema_with_env(config, file, &|name| std::env::var_os(name).is_some())
}

/// [`schema`] with the environment lookup injected (tests).
pub fn schema_with_env(
    config: &Config,
    file: Option<&DocumentMut>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Vec<SettingGroup> {
    let defaults = config_table(&Config::default());
    let current = config_table(config);
    let mut groups = Vec::new();
    walk(
        "",
        Some(&defaults),
        Some(&current),
        config,
        file,
        env_is_set,
        &mut groups,
    );
    let rank = |group: &SettingGroup| {
        let path = if group.id == "general" { "" } else { &group.id };
        GROUP_ORDER
            .iter()
            .position(|pattern| matches_pattern(pattern, path))
            .unwrap_or(GROUP_ORDER.len())
    };
    groups.sort_by_key(rank);
    groups
}

fn walk(
    prefix: &str,
    defaults: Option<&toml::Table>,
    current: Option<&toml::Table>,
    config: &Config,
    file: Option<&DocumentMut>,
    env_is_set: &dyn Fn(&str) -> bool,
    groups: &mut Vec<SettingGroup>,
) {
    let join = |key: &str| {
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        }
    };
    let mut keys: Vec<String> = Vec::new();
    for table in [defaults, current].into_iter().flatten() {
        for key in table.keys() {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    for (pattern, _) in OPTIONAL_KEYS {
        let (table, key) = pattern.rsplit_once('.').unwrap_or(("", pattern));
        if matches_pattern(table, prefix) && !keys.iter().any(|existing| existing == key) {
            keys.push(key.to_string());
        }
    }

    let mut fields = Vec::new();
    let mut subtables = Vec::new();
    for key in keys {
        let path = join(&key);
        let dv = defaults.and_then(|table| table.get(&key));
        let cv = current.and_then(|table| table.get(&key));
        if matches!(dv, Some(toml::Value::Table(_))) || matches!(cv, Some(toml::Value::Table(_))) {
            subtables.push((
                path,
                dv.and_then(toml::Value::as_table),
                cv.and_then(toml::Value::as_table),
            ));
            continue;
        }
        let kind = kind_for(&path, config, dv.or(cv), dv.is_some());
        let source = source_for(&path, file, env_is_set);
        let (current_text, default_text) = if kind == FieldKind::Secret {
            let set = cv
                .and_then(toml::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            (
                if set { "set" } else { "not set" }.to_string(),
                String::new(),
            )
        } else {
            (
                cv.map(render_toml).unwrap_or_default(),
                dv.map(render_toml).unwrap_or_default(),
            )
        };
        fields.push(SettingField {
            key: key.clone(),
            group: prefix.to_string(),
            kind,
            current: current_text,
            default: default_text,
            source,
            help: help_for(&path),
            restart_required: RESTART_REQUIRED.contains(&path.as_str()),
            path,
        });
    }
    if !fields.is_empty() {
        let provider = prefix
            .strip_prefix("providers.")
            .filter(|name| !name.contains('.'))
            .map(|name| ProviderCard {
                name: name.to_string(),
                referenced_by: config
                    .llm
                    .roles()
                    .into_iter()
                    .filter(|(_, provider)| *provider == name)
                    .map(|(role, _)| role)
                    .collect(),
                built_in: default_providers().contains_key(name),
            });
        groups.push(SettingGroup {
            id: if prefix.is_empty() {
                "general".to_string()
            } else {
                prefix.to_string()
            },
            title: if prefix.is_empty() {
                "General".to_string()
            } else {
                format!("[{prefix}]")
            },
            fields,
            provider,
        });
    }
    for (path, dv, cv) in subtables {
        walk(&path, dv, cv, config, file, env_is_set, groups);
    }
}

fn source_for(path: &str, file: Option<&DocumentMut>, env_is_set: &dyn Fn(&str) -> bool) -> Source {
    let name = env_name(path);
    if env_is_set(&name) {
        return Source::Env(name);
    }
    if path == "server.hmac_secret" && env_is_set(ENV_SECRET_ALIAS) {
        return Source::Env(ENV_SECRET_ALIAS.to_string());
    }
    if file.and_then(|doc| file_item(doc, path)).is_some() {
        Source::File
    } else {
        Source::Default
    }
}

// ---------------------------------------------------------------------------
// Writer (§13.2)
// ---------------------------------------------------------------------------

/// A posted value after typing by [`FieldKind`]; `Absent` removes an optional key.
#[derive(Debug, Clone, PartialEq)]
enum Typed {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<String>),
    Absent,
}

fn parse_posted(field: &SettingField, raw: &str) -> Result<Option<Typed>, String> {
    let optional = is_optional(&field.path);
    let trimmed = raw.trim();
    let err = |message: &str| Err(format!("{}: {message}", field.path));
    let typed = match &field.kind {
        FieldKind::Secret => return Ok(None),
        FieldKind::Bool => match trimmed {
            "true" | "on" | "1" => Typed::Bool(true),
            "false" | "off" | "0" => Typed::Bool(false),
            _ => return err("must be true or false"),
        },
        FieldKind::Integer => match trimmed.parse::<i64>() {
            Ok(value) => Typed::Int(value),
            Err(_) => return err("must be a whole number"),
        },
        FieldKind::Float => match trimmed.parse::<f64>() {
            Ok(value) if value.is_finite() => Typed::Float(round6(value)),
            _ => return err("must be a number"),
        },
        FieldKind::Enum(options) => {
            if !options.iter().any(|option| option == trimmed) {
                return err(&format!(
                    "must be one of {}",
                    options
                        .iter()
                        .map(|option| if option.is_empty() { "(empty)" } else { option })
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if trimmed.is_empty() && optional {
                Typed::Absent
            } else {
                Typed::Str(trimmed.to_string())
            }
        }
        FieldKind::Text | FieldKind::Path => {
            if trimmed.is_empty() && optional {
                Typed::Absent
            } else {
                Typed::Str(trimmed.to_string())
            }
        }
        FieldKind::TextList => Typed::List(
            raw.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    };
    Ok(Some(typed))
}

fn typed_from_edit(value: &Value, kind: &FieldKind) -> Option<Typed> {
    Some(match value {
        Value::Boolean(b) => Typed::Bool(*b.value()),
        Value::Integer(i) if *kind == FieldKind::Float => Typed::Float(*i.value() as f64),
        Value::Integer(i) => Typed::Int(*i.value()),
        Value::Float(f) => Typed::Float(round6(*f.value())),
        Value::String(s) => Typed::Str(s.value().clone()),
        Value::Array(items) => Typed::List(
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
        ),
        Value::Datetime(_) | Value::InlineTable(_) => return None,
    })
}

fn typed_from_toml(value: &toml::Value, kind: &FieldKind) -> Option<Typed> {
    Some(match value {
        toml::Value::Boolean(b) => Typed::Bool(*b),
        toml::Value::Integer(i) if *kind == FieldKind::Float => Typed::Float(*i as f64),
        toml::Value::Integer(i) => Typed::Int(*i),
        toml::Value::Float(f) => Typed::Float(round6(*f)),
        toml::Value::String(s) => Typed::Str(s.clone()),
        toml::Value::Array(items) => Typed::List(
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
        ),
        _ => return None,
    })
}

fn edit_value(typed: &Typed) -> Option<Value> {
    Some(match typed {
        Typed::Bool(b) => Value::from(*b),
        Typed::Int(i) => Value::from(*i),
        Typed::Float(f) => Value::from(*f),
        Typed::Str(s) => Value::from(s.as_str()),
        Typed::List(items) => {
            let mut array = Array::new();
            for item in items {
                array.push_formatted(Value::from(item.as_str()).decorated("\n  ", ""));
            }
            if !items.is_empty() {
                array.set_trailing_comma(true);
                array.set_trailing("\n");
            }
            Value::Array(array)
        }
        Typed::Absent => return None,
    })
}

/// The TOML literal of a value without its surrounding whitespace or comments.
fn literal(value: &Value) -> String {
    let mut bare = value.clone();
    bare.decor_mut().clear();
    bare.to_string().trim().to_string()
}

/// One key the writer changed, with the literals for `config_changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedKey {
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// A change planned against the file; applied by [`apply_change`].
#[derive(Debug, Clone)]
struct PlannedChange {
    path: String,
    value: Typed,
}

/// Compare every posted field with what the file holds (or the default when
/// the file lacks the key) and keep the ones that differ (§13.2 item 2).
/// Field errors are collected, never partial.
fn plan_changes(
    groups: &[SettingGroup],
    doc: &DocumentMut,
    posted: &[(String, String)],
) -> Result<Vec<PlannedChange>, Vec<String>> {
    let defaults = config_table(&Config::default());
    let mut errors = Vec::new();
    let mut changes = Vec::new();
    for group in groups {
        for field in &group.fields {
            if field.is_env() || field.is_secret() {
                continue;
            }
            // Last value wins when a name is posted twice.
            let Some((_, raw)) = posted.iter().rev().find(|(name, _)| *name == field.path) else {
                continue;
            };
            let typed = match parse_posted(field, raw) {
                Ok(Some(typed)) => typed,
                Ok(None) => continue,
                Err(message) => {
                    errors.push(message);
                    continue;
                }
            };
            let default = lookup_toml(&defaults, &field.path)
                .and_then(|value| typed_from_toml(value, &field.kind));
            if typed == Typed::Absent && default.is_some() {
                errors.push(format!(
                    "{}: the built-in default is {}; it can be changed but not unset",
                    field.path, field.default
                ));
                continue;
            }
            let in_file = file_item(doc, &field.path)
                .and_then(Item::as_value)
                .and_then(|value| typed_from_edit(value, &field.kind));
            let effective = match in_file {
                Some(value) => value,
                None => default.unwrap_or(Typed::Absent),
            };
            if effective != typed {
                changes.push(PlannedChange {
                    path: field.path.clone(),
                    value: typed,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(changes)
    } else {
        Err(errors)
    }
}

fn lookup_toml<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut segments = path.split('.');
    let mut value = table.get(segments.next()?)?;
    for segment in segments {
        value = value.as_table()?.get(segment)?;
    }
    Some(value)
}

/// Walk to the table holding `path`'s leaf, creating intermediate tables as
/// implicit ones and the leaf's own table as an explicit `[header]`.
fn leaf_table<'a>(doc: &'a mut DocumentMut, path: &str) -> (&'a mut Table, String) {
    let mut segments: Vec<&str> = path.split('.').collect();
    let key = segments.pop().unwrap_or_default().to_string();
    let mut table = doc.as_table_mut();
    let depth = segments.len();
    for (index, segment) in segments.into_iter().enumerate() {
        let is_leaf_parent = index + 1 == depth;
        let item = table.entry(segment).or_insert_with(|| {
            let mut fresh = Table::new();
            fresh.set_implicit(!is_leaf_parent);
            Item::Table(fresh)
        });
        if !item.is_table() {
            // `None`, or a value where a table is needed: replace it (the
            // loader would have rejected such a file anyway).
            let mut fresh = Table::new();
            fresh.set_implicit(!is_leaf_parent);
            *item = Item::Table(fresh);
        }
        table = match item.as_table_mut() {
            Some(table) => table,
            None => unreachable!("item was just made a table"),
        };
        if is_leaf_parent && table.is_implicit() {
            table.set_implicit(false);
        }
    }
    (table, key)
}

/// Set one leaf in the document, keeping the key's comments and the value's
/// trailing comment; returns the before/after literals.
fn apply_change(doc: &mut DocumentMut, change: &PlannedChange) -> ChangedKey {
    let (table, key) = leaf_table(doc, &change.path);
    let old = table.get(&key).and_then(Item::as_value).map(literal);
    let new = match edit_value(&change.value) {
        None => {
            table.remove(&key);
            None
        }
        Some(mut value) => {
            if let Some(existing) = table.get_mut(&key).and_then(Item::as_value_mut) {
                *value.decor_mut() = existing.decor().clone();
                *existing = value;
                table.get(&key).and_then(Item::as_value).map(literal)
            } else {
                let rendered = literal(&value);
                table.insert(&key, Item::Value(value));
                Some(rendered)
            }
        }
    };
    ChangedKey {
        key: change.path.clone(),
        old,
        new,
    }
}

/// Parse `path` into a document; a missing file starts empty.
pub fn read_document(path: &Path) -> Result<DocumentMut, String> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    DocumentMut::from_str(&raw)
        .map_err(|error| format!("{} does not parse: {error}", path.display()))
}

/// Render `doc` to `<path>.tmp.<pid>`, validate it with [`Config::load`],
/// copy the original's permissions and rename it over the original (§13.2
/// item 3). On any failure the original is untouched and the temp file is gone.
pub fn write_validated(path: &Path, doc: &DocumentMut) -> Result<Config, String> {
    let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    let cleanup = |tmp: &Path| {
        let _ = std::fs::remove_file(tmp);
    };
    std::fs::write(&tmp, doc.to_string()).map_err(|error| {
        cleanup(&tmp);
        format!("could not write {}: {error}", tmp.display())
    })?;
    let config = match Config::load(Some(&tmp)) {
        Ok(config) => config,
        Err(error) => {
            cleanup(&tmp);
            return Err(error.to_string());
        }
    };
    if let Ok(metadata) = std::fs::metadata(path)
        && let Err(error) = std::fs::set_permissions(&tmp, metadata.permissions())
    {
        cleanup(&tmp);
        return Err(format!(
            "could not copy permissions onto {}: {error}",
            tmp.display()
        ));
    }
    std::fs::rename(&tmp, path).map_err(|error| {
        cleanup(&tmp);
        format!("could not replace {}: {error}", path.display())
    })?;
    Ok(config)
}

/// What a save attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The file was written and the live config swapped.
    Saved {
        changed: Vec<ChangedKey>,
        restart_required: Vec<String>,
    },
    /// Nothing was written; the messages go back to the form.
    Rejected(Vec<String>),
}

fn config_path(state: &AppState) -> Result<PathBuf, WebError> {
    state.config_path.clone().ok_or_else(|| {
        WebError::BadRequest("no config file is configured; start the server with --config".into())
    })
}

/// Swap the live config after a successful write and remember the new mtime.
fn install(state: &AppState, path: &Path, config: Config) {
    match state.config.write() {
        Ok(mut live) => *live = std::sync::Arc::new(config),
        Err(poisoned) => *poisoned.into_inner() = std::sync::Arc::new(config),
    }
    let mtime = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    match state.web.config_mtime.lock() {
        Ok(mut cached) => *cached = mtime,
        Err(poisoned) => *poisoned.into_inner() = mtime,
    }
}

async fn record_changes(
    state: &AppState,
    user_id: i64,
    changed: &[ChangedKey],
) -> Result<(), WebError> {
    let now = crate::db::fmt_ts(jiff::Timestamp::now());
    for change in changed {
        sqlx::query(
            "INSERT INTO config_changes (user_id, key, old_value, new_value, changed_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(&change.key)
        .bind(change.old.as_deref())
        .bind(change.new.as_deref())
        .bind(&now)
        .execute(state.db.pool())
        .await
        .map_err(crate::db::DbError::from)?;
    }
    Ok(())
}

/// Write the changed fields of a settings form (§13.2): plan against the
/// file, write through [`write_validated`], record `config_changes`, swap the
/// live config.
pub async fn save_settings(
    state: &AppState,
    user_id: i64,
    posted: &[(String, String)],
) -> Result<SaveOutcome, WebError> {
    let path = config_path(state)?;
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    let groups = schema(&state.config(), Some(&doc));
    let planned = match plan_changes(&groups, &doc, posted) {
        Ok(planned) => planned,
        Err(errors) => return Ok(SaveOutcome::Rejected(errors)),
    };
    if planned.is_empty() {
        return Ok(SaveOutcome::Saved {
            changed: Vec::new(),
            restart_required: Vec::new(),
        });
    }
    let changed: Vec<ChangedKey> = planned
        .iter()
        .map(|change| apply_change(&mut doc, change))
        .collect();
    let config = match write_validated(&path, &doc) {
        Ok(config) => config,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    record_changes(state, user_id, &changed).await?;
    install(state, &path, config);
    let restart_required = changed
        .iter()
        .filter(|change| RESTART_REQUIRED.contains(&change.key.as_str()))
        .map(|change| change.key.clone())
        .collect();
    Ok(SaveOutcome::Saved {
        changed,
        restart_required,
    })
}

// ---------------------------------------------------------------------------
// Providers (§13.3)
// ---------------------------------------------------------------------------

fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn provider_literal(name: &str, table: &Table) -> String {
    // A provider key is supposed to come from the environment, but Config
    // still accepts `api_key` in TOML. Never copy a hand-written key into the
    // audit table (and therefore onto the history page) when removing a
    // provider.
    let mut redacted = table.clone();
    redacted.remove("api_key");
    format!("[providers.{name}]\n{redacted}")
        .trim_end()
        .to_string()
}

/// Insert `[providers.<name>]` with `ProviderConfig::default()`, the chosen
/// kind and placeholders, through the validate-and-rename path.
pub async fn add_provider(
    state: &AppState,
    user_id: i64,
    name: &str,
    kind: ProviderKind,
) -> Result<SaveOutcome, WebError> {
    let path = config_path(state)?;
    if !valid_provider_name(name) {
        return Ok(SaveOutcome::Rejected(vec![format!(
            "provider name {name:?} must be lowercase ascii letters, digits or '_'"
        )]));
    }
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    if state.config().providers.contains_key(name)
        || file_item(&doc, &format!("providers.{name}")).is_some()
    {
        return Ok(SaveOutcome::Rejected(vec![format!(
            "providers.{name} already exists"
        )]));
    }
    let provider = ProviderConfig {
        kind,
        base_url: "https://api.example.com/v1".into(),
        model: "model-name".into(),
        ..ProviderConfig::default()
    };
    let rendered = toml::to_string(&provider)
        .map_err(|error| WebError::Internal(anyhow::anyhow!("serializing provider: {error}")))?;
    let mut table = DocumentMut::from_str(&rendered)
        .map_err(|error| WebError::Internal(anyhow::anyhow!("parsing provider: {error}")))?
        .into_table();
    table.set_implicit(false);
    let literal = provider_literal(name, &table);
    {
        let providers = doc.as_table_mut().entry("providers").or_insert_with(|| {
            let mut fresh = Table::new();
            fresh.set_implicit(true);
            Item::Table(fresh)
        });
        let Some(providers) = providers.as_table_mut() else {
            return Ok(SaveOutcome::Rejected(vec![
                "providers is not a table in the config file".into(),
            ]));
        };
        providers.insert(name, Item::Table(table));
    }
    let config = match write_validated(&path, &doc) {
        Ok(config) => config,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    let changed = vec![ChangedKey {
        key: format!("providers.{name}"),
        old: None,
        new: Some(literal),
    }];
    record_changes(state, user_id, &changed).await?;
    install(state, &path, config);
    Ok(SaveOutcome::Saved {
        changed,
        restart_required: Vec::new(),
    })
}

/// Remove `[providers.<name>]`; refused while an `[llm]` role names it, and
/// for the shipped providers (`Config::default()` declares them, so deleting
/// their table would only reset them to the built-in values).
pub async fn remove_provider(
    state: &AppState,
    user_id: i64,
    name: &str,
) -> Result<SaveOutcome, WebError> {
    let path = config_path(state)?;
    let config = state.config();
    let roles: Vec<&str> = config
        .llm
        .roles()
        .into_iter()
        .filter(|(_, provider)| *provider == name)
        .map(|(role, _)| role)
        .collect();
    if !roles.is_empty() {
        return Ok(SaveOutcome::Rejected(vec![format!(
            "providers.{name} is named by llm.{}; reassign the role first",
            roles.join(" and llm.")
        )]));
    }
    if default_providers().contains_key(name) {
        return Ok(SaveOutcome::Rejected(vec![format!(
            "providers.{name} is built in (declared by the defaults); it cannot be removed, \
             only left unreferenced"
        )]));
    }
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    let removed = doc
        .as_table_mut()
        .get_mut("providers")
        .and_then(Item::as_table_mut)
        .and_then(|providers| providers.remove(name))
        .and_then(|item| item.into_table().ok());
    let Some(removed) = removed else {
        return Ok(SaveOutcome::Rejected(vec![format!(
            "providers.{name} is not declared in the config file"
        )]));
    };
    let config = match write_validated(&path, &doc) {
        Ok(config) => config,
        Err(message) => return Ok(SaveOutcome::Rejected(vec![message])),
    };
    let changed = vec![ChangedKey {
        key: format!("providers.{name}"),
        old: Some(provider_literal(name, &removed)),
        new: None,
    }];
    record_changes(state, user_id, &changed).await?;
    install(state, &path, config);
    Ok(SaveOutcome::Saved {
        changed,
        restart_required: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard/settings.html")]
struct SettingsTemplate {
    page: Page,
    groups: Vec<SettingGroup>,
    config_path: Option<String>,
    load_error: Option<String>,
    errors: Vec<String>,
    provider_kinds: Vec<String>,
}

fn viewer_of(user: Option<crate::web::users::User>, next: &str) -> Result<Viewer, WebError> {
    user.map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: next.to_string(),
        })
}

fn render_settings(
    state: &AppState,
    viewer: Viewer,
    flash: Option<Flash>,
    load_error: Option<String>,
    errors: Vec<String>,
    posted: &[(String, String)],
    status: StatusCode,
) -> Response {
    let config = state.config();
    let doc = state
        .config_path
        .as_deref()
        .and_then(|path| read_document(path).ok());
    let mut groups = schema(&config, doc.as_ref());
    for group in &mut groups {
        for field in &mut group.fields {
            if field.is_env() || field.is_secret() {
                continue;
            }
            if let Some((_, raw)) = posted.iter().rev().find(|(name, _)| *name == field.path) {
                field.current = raw.clone();
            }
        }
    }
    let mut page = Page::new("Settings", Some(viewer), "settings");
    page.flash = flash;
    let mut response = Html(SettingsTemplate {
        page,
        groups,
        config_path: state
            .config_path
            .as_ref()
            .map(|path| path.display().to_string()),
        load_error,
        errors,
        provider_kinds: strings(PROVIDER_KINDS),
    })
    .into_response();
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

/// `GET /dashboard/settings` (§13.1): reload the file when its mtime changed,
/// then render every key.
async fn show(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    let viewer = viewer_of(auth.user().await, "/dashboard/settings")?;
    let load_error = WebState::reload_if_changed(&state)
        .err()
        .map(|error| error.to_string());
    let flash = take_flash(&session).await?;
    Ok(render_settings(
        &state,
        viewer,
        flash,
        load_error,
        Vec::new(),
        &[],
        StatusCode::OK,
    ))
}

async fn respond(
    state: &AppState,
    session: &Session,
    viewer: Viewer,
    outcome: SaveOutcome,
    posted: &[(String, String)],
) -> Result<Response, WebError> {
    match outcome {
        SaveOutcome::Saved {
            changed,
            restart_required,
        } => {
            let mut text = if changed.is_empty() {
                "Nothing changed.".to_string()
            } else {
                format!(
                    "Saved {} setting{} — they apply to the next run.",
                    changed.len(),
                    if changed.len() == 1 { "" } else { "s" }
                )
            };
            if !restart_required.is_empty() {
                text.push_str(&format!(
                    " Restart the server for: {}.",
                    restart_required.join(", ")
                ));
            }
            session
                .insert(
                    "flash",
                    Flash {
                        kind: "success".into(),
                        text,
                    },
                )
                .await
                .map_err(|error| WebError::Internal(error.into()))?;
            Ok(Redirect::to("/dashboard/settings").into_response())
        }
        SaveOutcome::Rejected(errors) => Ok(render_settings(
            state,
            viewer,
            None,
            None,
            errors,
            posted,
            StatusCode::BAD_REQUEST,
        )),
    }
}

/// `POST /dashboard/settings` (§13.2).
async fn save(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    body: Bytes,
) -> Result<Response, WebError> {
    let viewer = viewer_of(auth.user().await, "/dashboard/settings")?;
    config_path(&state)?;
    // A file that no longer loads is reported by the GET; the save below
    // re-reads the document from disk regardless.
    let _ = WebState::reload_if_changed(&state);
    let posted: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let outcome = save_settings(&state, viewer.id, &posted).await?;
    respond(&state, &session, viewer, outcome, &posted).await
}

#[derive(Debug, Deserialize)]
struct ProviderForm {
    action: String,
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

/// `POST /dashboard/settings/providers` (§13.3): `action=add|remove`.
async fn providers(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Form(form): Form<ProviderForm>,
) -> Result<Response, WebError> {
    let viewer = viewer_of(auth.user().await, "/dashboard/settings")?;
    config_path(&state)?;
    let _ = WebState::reload_if_changed(&state);
    let name = form.name.trim();
    let outcome = match form.action.as_str() {
        "add" => {
            let kind = match form.kind.as_deref().map(str::trim) {
                Some("openai") => ProviderKind::OpenAi,
                Some("anthropic") => ProviderKind::Anthropic,
                _ => {
                    return Err(WebError::BadRequest(
                        "kind must be openai or anthropic".into(),
                    ));
                }
            };
            add_provider(&state, viewer.id, name, kind).await?
        }
        "remove" => remove_provider(&state, viewer.id, name).await?,
        _ => return Err(WebError::BadRequest("action must be add or remove".into())),
    };
    respond(&state, &session, viewer, outcome, &[]).await
}

// ---------------------------------------------------------------------------
// History (§13.4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConfigChange {
    pub id: i64,
    pub key: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub changed_at: String,
    pub username: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/settings_history.html")]
struct HistoryTemplate {
    page: Page,
    changes: Vec<ConfigChange>,
    pagination: Pagination,
}

#[derive(Debug, Default, Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    page: Option<u32>,
}

const HISTORY_PER_PAGE: u32 = 100;

/// `config_changes` newest first, `HISTORY_PER_PAGE` per page.
pub async fn config_changes(
    state: &AppState,
    page: u32,
) -> Result<(Vec<ConfigChange>, Pagination), WebError> {
    let config = state.config();
    let total: i64 = sqlx::query("SELECT COUNT(*) AS n FROM config_changes")
        .fetch_one(state.db.pool())
        .await
        .map_err(crate::db::DbError::from)?
        .get("n");
    let pagination = Pagination {
        page: page.max(1),
        per_page: HISTORY_PER_PAGE,
        total,
    };
    let rows = sqlx::query(
        "SELECT c.id, c.key, c.old_value, c.new_value, c.changed_at, u.username
         FROM config_changes c
         LEFT JOIN users u ON u.id = c.user_id
         ORDER BY c.changed_at DESC, c.id DESC
         LIMIT ? OFFSET ?",
    )
    .bind(i64::from(HISTORY_PER_PAGE))
    .bind(pagination.offset())
    .fetch_all(state.db.pool())
    .await
    .map_err(crate::db::DbError::from)?;
    let changes = rows
        .into_iter()
        .map(|row| {
            let changed_at: String = row.get("changed_at");
            ConfigChange {
                id: row.get("id"),
                key: row.get("key"),
                old_value: row.get("old_value"),
                new_value: row.get("new_value"),
                changed_at: crate::db::parse_ts("config_changes.changed_at", &changed_at)
                    .map(|ts| format_time(ts, &config))
                    .unwrap_or(changed_at),
                username: row.get("username"),
            }
        })
        .collect();
    Ok((changes, pagination))
}

/// `GET /dashboard/settings/history`.
async fn history(
    State(state): State<AppState>,
    auth: AuthSession,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, WebError> {
    let viewer = viewer_of(auth.user().await, "/dashboard/settings/history")?;
    let (changes, pagination) = config_changes(&state, query.page.unwrap_or(1)).await?;
    Ok(Html(HistoryTemplate {
        page: Page::new("Settings history", Some(viewer), "settings"),
        changes,
        pagination,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    use super::*;
    use crate::db::Db;
    use crate::server::router;
    use crate::web::users;

    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")
    }

    /// Every leaf path of a TOML table, depth first.
    fn leaves(prefix: &str, table: &toml::Table, out: &mut Vec<String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => leaves(&path, inner, out),
                _ => out.push(path),
            }
        }
    }

    fn fields(groups: &[SettingGroup]) -> Vec<&SettingField> {
        groups
            .iter()
            .flat_map(|group| group.fields.iter())
            .collect()
    }

    fn field<'a>(groups: &'a [SettingGroup], path: &str) -> &'a SettingField {
        fields(groups)
            .into_iter()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("{path} missing from the schema"))
    }

    async fn test_state(config_file: Option<&Path>) -> (tempfile::TempDir, AppState, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let config = match config_file {
            Some(path) => Config::load(Some(path)).unwrap(),
            None => Config::default(),
        };
        let state = AppState::new(db, config, config_file.map(Path::to_path_buf));
        let admin = users::add(&state.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        (dir, state, admin.id)
    }

    fn copy_example(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::copy(example_path(), &path).unwrap();
        path
    }

    fn posted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Push the file's mtime into the future so a rewrite within the same
    /// timestamp tick still counts as a change.
    fn touch_later(path: &Path, seconds: u64) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(seconds);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    #[test]
    fn every_default_leaf_appears_exactly_once() {
        let groups = schema(&Config::default(), None);
        let mut expected = Vec::new();
        leaves("", &config_table(&Config::default()), &mut expected);
        assert!(expected.len() > 80, "{}", expected.len());
        let all = fields(&groups);
        for path in &expected {
            let count = all.iter().filter(|field| field.path == *path).count();
            assert_eq!(count, 1, "{path} appears {count} times");
        }
        let mut seen = std::collections::HashSet::new();
        for field in &all {
            assert!(seen.insert(&field.path), "{} duplicated", field.path);
            assert_eq!(
                field.group,
                field.path.rsplit_once('.').map(|(g, _)| g).unwrap_or(""),
            );
        }
        // Optional keys the defaults leave unset still get a field.
        for path in [
            "providers.deepseek.api_key",
            "providers.deepseek.effort",
            "server.hmac_secret",
            "xtc.settings",
            "server.basic_auth_user",
            "bookorbit.opds_user",
            "bookorbit.opds_pass",
            "mail.smtp_user",
            "mail.smtp_pass",
            "mail.notify_to",
        ] {
            field(&groups, path);
        }
    }

    #[test]
    fn groups_render_in_the_plan_order_with_anchors() {
        let groups = schema(&Config::default(), None);
        let ids: Vec<&str> = groups.iter().map(|group| group.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "general",
                "llm",
                "providers.anthropic",
                "providers.deepseek",
                "providers.gemini",
                "voyage",
                "curation",
                "curation.feedback",
                "curation.ranking",
                "curation.ranking.quotas",
                "curation.ranking.weights.preliminary",
                "curation.ranking.weights.utility",
                "curation.ranking.diversity",
                "editorial",
                "publish",
                "xtc",
                "server",
                "miniflux",
                "bookorbit",
                "mail",
                "discovery",
            ]
        );
        let anthropic = groups
            .iter()
            .find(|group| group.id == "providers.anthropic")
            .unwrap();
        assert_eq!(
            anthropic.provider.as_ref().unwrap().referenced_by,
            vec!["editor"]
        );
        let gemini = groups
            .iter()
            .find(|group| group.id == "providers.gemini")
            .unwrap();
        assert!(gemini.provider.as_ref().unwrap().referenced_by.is_empty());
        assert!(
            groups
                .iter()
                .find(|group| group.id == "curation")
                .unwrap()
                .provider
                .is_none()
        );
    }

    #[test]
    fn every_shipped_key_and_every_field_has_help() {
        let raw = std::fs::read_to_string(example_path()).unwrap();
        let table: toml::Table = toml::from_str(&raw).unwrap();
        let mut shipped = Vec::new();
        leaves("", &table, &mut shipped);
        assert!(shipped.len() > 80, "{}", shipped.len());
        for path in &shipped {
            assert!(help_for(path).is_some(), "no help for {path}");
        }
        for field in fields(&schema(&Config::default(), None)) {
            assert!(field.help.is_some(), "no help for {}", field.path);
        }
    }

    #[test]
    fn kinds_follow_the_static_table() {
        let groups = schema(&Config::default(), None);
        assert_eq!(field(&groups, "world_briefing").kind, FieldKind::Bool);
        assert_eq!(field(&groups, "lookback_hours").kind, FieldKind::Integer);
        assert_eq!(
            field(&groups, "llm.score_temperature").kind,
            FieldKind::Float
        );
        assert_eq!(field(&groups, "llm.score_temperature").current, "0.3");
        assert_eq!(field(&groups, "timezone").kind, FieldKind::Text);
        assert_eq!(
            field(&groups, "curation.sections").kind,
            FieldKind::TextList
        );
        assert!(
            field(&groups, "curation.sections")
                .current
                .starts_with("Top Stories\nTech & Engineering\n")
        );
        assert_eq!(field(&groups, "database_path").kind, FieldKind::Path);
        assert_eq!(field(&groups, "xtc.settings").kind, FieldKind::Path);
        assert_eq!(
            field(&groups, "providers.deepseek.kind").kind,
            FieldKind::Enum(strings(PROVIDER_KINDS))
        );
        assert_eq!(
            field(&groups, "providers.anthropic.effort").kind,
            FieldKind::Enum(strings(&ANTHROPIC_EFFORTS[1..]))
        );
        let mut with_new = Config::default();
        with_new.providers.insert(
            "fresh".into(),
            ProviderConfig {
                kind: ProviderKind::Anthropic,
                ..ProviderConfig::default()
            },
        );
        assert_eq!(
            field(&schema(&with_new, None), "providers.fresh.effort").kind,
            FieldKind::Enum(strings(ANTHROPIC_EFFORTS))
        );
        assert_eq!(field(&groups, "providers.anthropic.effort").current, "high");
        assert_eq!(
            field(&groups, "providers.deepseek.effort").kind,
            FieldKind::Text
        );
        assert_eq!(
            field(&groups, "llm.bulk").kind,
            FieldKind::Enum(strings(&["anthropic", "deepseek", "gemini"]))
        );
        assert_eq!(
            field(&groups, "llm.editor").kind,
            FieldKind::Enum(strings(&["", "anthropic", "deepseek", "gemini"]))
        );
        assert_eq!(
            field(&groups, "editorial.summary_model").kind,
            FieldKind::Enum(strings(SUMMARY_MODELS))
        );
        assert_eq!(
            field(&groups, "xtc.format").kind,
            FieldKind::Enum(strings(XTC_FORMATS))
        );
        assert!(field(&groups, "server.bind").restart_required);
        assert!(field(&groups, "database_path").restart_required);
        assert!(!field(&groups, "lookback_hours").restart_required);
    }

    #[test]
    fn secrets_never_carry_a_value() {
        let mut config = Config::default();
        config.miniflux.api_key = Some("hunter2-miniflux".into());
        config.voyage.api_key = Some("hunter2-voyage".into());
        config.server.hmac_secret = Some("hunter2-hmac".into());
        config.server.basic_auth_pass = Some("hunter2-basic".into());
        config.bookorbit.opds_pass = Some("hunter2-bookorbit".into());
        config.mail.smtp_pass = Some("hunter2-smtp".into());
        if let Some(provider) = config.providers.get_mut("deepseek") {
            provider.api_key = Some("hunter2-deepseek".into());
        }
        let groups = schema_with_env(&config, None, &|_| false);
        for path in [
            "miniflux.api_key",
            "voyage.api_key",
            "server.hmac_secret",
            "server.basic_auth_pass",
            "bookorbit.opds_pass",
            "mail.smtp_pass",
            "providers.deepseek.api_key",
            "providers.anthropic.api_key",
        ] {
            let field = field(&groups, path);
            assert_eq!(field.kind, FieldKind::Secret, "{path}");
            assert!(field.default.is_empty());
        }
        assert_eq!(field(&groups, "miniflux.api_key").current, "set");
        assert_eq!(
            field(&groups, "providers.anthropic.api_key").current,
            "not set"
        );
        for field in fields(&groups) {
            assert!(!field.current.contains("hunter2"), "{}", field.path);
            assert!(!field.default.contains("hunter2"), "{}", field.path);
        }
    }

    #[test]
    fn env_detection_uses_the_derived_name() {
        let name = "DAILY_EPUB_CURATION__RANKING__TELEMETRY_RETENTION_DAYS";
        assert_eq!(env_name("curation.ranking.telemetry_retention_days"), name);
        assert_eq!(
            env_name("providers.gemini.api_key"),
            ProviderConfig::api_key_env_var("gemini")
        );
        // Never mutate the process environment here: other tests load the
        // shipped config through figment concurrently and would see the
        // variable. The probe is injected instead.
        let groups = schema_with_env(&Config::default(), None, &|var| var == name);
        assert_eq!(
            field(&groups, "curation.ranking.telemetry_retention_days").source,
            Source::Env(name.to_string())
        );
        assert_eq!(
            field(&groups, "curation.ranking.embedding_retention_days").source,
            Source::Default
        );

        let groups = schema_with_env(&Config::default(), None, &|var| var == ENV_SECRET_ALIAS);
        assert_eq!(
            field(&groups, "server.hmac_secret").source,
            Source::Env(ENV_SECRET_ALIAS.to_string())
        );
        let doc =
            DocumentMut::from_str("lookback_hours = 30\n[llm]\nbulk = \"deepseek\"\n").unwrap();
        let groups = schema_with_env(&Config::default(), Some(&doc), &|_| false);
        assert_eq!(field(&groups, "lookback_hours").source, Source::File);
        assert_eq!(field(&groups, "llm.bulk").source, Source::File);
        assert_eq!(field(&groups, "llm.editor").source, Source::Default);
    }

    #[test]
    fn enum_options_round_trip_through_validate() {
        let groups = schema(&Config::default(), None);
        let mut checked = 0;
        for field in fields(&groups) {
            let FieldKind::Enum(options) = &field.kind else {
                continue;
            };
            for option in options {
                let mut table = config_table(&Config::default());
                let mut segments: Vec<&str> = field.path.split('.').collect();
                let key = segments.pop().unwrap();
                let mut target = &mut table;
                for segment in segments {
                    target = target
                        .get_mut(segment)
                        .and_then(toml::Value::as_table_mut)
                        .unwrap();
                }
                if option.is_empty() && is_optional(&field.path) {
                    target.remove(key);
                } else {
                    target.insert(key.to_string(), toml::Value::String(option.clone()));
                }
                let config: Config = toml::Value::Table(table)
                    .try_into()
                    .unwrap_or_else(|error| panic!("{} = {option:?}: {error}", field.path));
                config
                    .validate()
                    .unwrap_or_else(|error| panic!("{} = {option:?}: {error}", field.path));
                checked += 1;
            }
        }
        assert!(checked >= 20, "{checked}");
    }

    #[test]
    fn floats_render_with_at_most_six_decimals() {
        assert_eq!(fmt_float(0.3), "0.3");
        assert_eq!(fmt_float(60.0), "60.0");
        assert_eq!(fmt_float(0.0028), "0.0028");
        assert_eq!(fmt_float(0.1 + 0.2), "0.3");
        assert_eq!(fmt_float(1.23456789), "1.234568");
    }

    #[tokio::test]
    async fn changing_three_keys_preserves_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let (_db_dir, state, admin) = test_state(Some(&path)).await;
        let original = std::fs::read_to_string(&path).unwrap();
        let sections = field(&schema(&state.config(), None), "curation.sections")
            .current
            .clone();
        let outcome = save_settings(
            &state,
            admin,
            &posted(&[
                ("lookback_hours", "30"),
                ("curation.ranking.deep_keep", "150"),
                ("xtc.format", "xtc"),
                // Unchanged values are skipped even when posted.
                ("timezone", "America/New_York"),
                ("curation.ranking.rating_half_life_days", "60"),
                ("llm.score_temperature", "0.3"),
                ("curation.sections", &sections),
            ]),
        )
        .await
        .unwrap();
        let SaveOutcome::Saved {
            changed,
            restart_required,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            changed,
            vec![
                ChangedKey {
                    key: "lookback_hours".into(),
                    old: Some("26".into()),
                    new: Some("30".into()),
                },
                ChangedKey {
                    key: "curation.ranking.deep_keep".into(),
                    old: Some("120".into()),
                    new: Some("150".into()),
                },
                ChangedKey {
                    key: "xtc.format".into(),
                    old: Some("\"xtch\"".into()),
                    new: Some("\"xtc\"".into()),
                },
            ]
        );
        assert!(restart_required.is_empty());

        let saved = std::fs::read_to_string(&path).unwrap();
        let before: Vec<&str> = original.lines().collect();
        let after: Vec<&str> = saved.lines().collect();
        assert_eq!(before.len(), after.len());
        let mut differing = Vec::new();
        for (index, (old, new)) in before.iter().zip(&after).enumerate() {
            if old != new {
                differing.push((index, *old, *new));
            }
        }
        assert_eq!(differing.len(), 3, "{differing:?}");
        assert_eq!(differing[0].2, "lookback_hours = 30");
        assert_eq!(
            differing[1].2,
            "deep_keep = 150                      # deep-assessment set"
        );
        assert_eq!(
            differing[2].2,
            "format = \"xtc\"                      # xtc (1-bit) | xtch (grayscale)"
        );
        assert_eq!(
            original.lines().filter(|line| line.contains('#')).count(),
            saved.lines().filter(|line| line.contains('#')).count()
        );
        assert_eq!(state.config().lookback_hours, 30);
        assert_eq!(state.config().curation.ranking.deep_keep, 150);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let (changes, pagination) = config_changes(&state, 1).await.unwrap();
        assert_eq!(pagination.total, 3);
        assert_eq!(changes[0].key, "xtc.format");
        assert_eq!(changes[0].old_value.as_deref(), Some("\"xtch\""));
        assert_eq!(changes[0].new_value.as_deref(), Some("\"xtc\""));
        assert_eq!(changes[0].username.as_deref(), Some("admin"));
        assert_eq!(changes[2].key, "lookback_hours");
    }

    #[tokio::test]
    async fn a_new_key_in_an_absent_table_creates_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# minimal\nlookback_hours = 26\n").unwrap();
        let (_db_dir, state, admin) = test_state(Some(&path)).await;
        let outcome = save_settings(
            &state,
            admin,
            &posted(&[
                ("curation.feedback.good_value", "0.5"),
                ("curation.ranking.quotas.knn", "25"),
                ("world_briefing", "false"),
                ("server.bind", "127.0.0.1:3500"),
            ]),
        )
        .await
        .unwrap();
        let SaveOutcome::Saved {
            changed,
            restart_required,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(changed.len(), 4);
        let good = changed
            .iter()
            .find(|c| c.key == "curation.feedback.good_value")
            .unwrap();
        assert_eq!(good.old, None);
        assert_eq!(good.new.as_deref(), Some("0.5"));
        assert_eq!(restart_required, vec!["server.bind".to_string()]);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with("# minimal\nlookback_hours = 26\nworld_briefing = false\n"),
            "{saved}"
        );
        assert!(
            saved.contains("[curation.feedback]\ngood_value = 0.5\n"),
            "{saved}"
        );
        assert!(
            saved.contains("[curation.ranking.quotas]\nknn = 25\n"),
            "{saved}"
        );
        assert!(
            saved.contains("[server]\nbind = \"127.0.0.1:3500\"\n"),
            "{saved}"
        );
        assert!(!saved.contains("[curation]\n"), "{saved}");
        let reloaded = Config::load(Some(&path)).unwrap();
        assert_eq!(reloaded.curation.feedback.good_value, 0.5);
        assert_eq!(reloaded.curation.ranking.quotas.knn, 25);
        assert!(!reloaded.world_briefing);
        assert_eq!(state.config().server.bind, "127.0.0.1:3500");
    }

    #[tokio::test]
    async fn text_lists_write_multi_line_arrays_and_optional_keys_can_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let (_db_dir, state, admin) = test_state(Some(&path)).await;
        let outcome = save_settings(
            &state,
            admin,
            &posted(&[
                ("curation.blocked_domains", " a.example \n\nb.example\n"),
                ("curation.sections", "Top Stories\nNiche Corner"),
                ("providers.anthropic.effort", "low"),
                ("xtc.settings", ""),
                ("llm.editor", ""),
            ]),
        )
        .await
        .unwrap();
        let SaveOutcome::Saved { changed, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(changed.len(), 5);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("blocked_domains = [\n  \"a.example\",\n  \"b.example\",\n]\n"),
            "{saved}"
        );
        assert!(
            saved.contains(
                "sections = [\n  \"Top Stories\",\n  \"Niche Corner\",\n]\n\n[curation.feedback]"
            ),
            "{saved}"
        );
        assert!(
            saved
                .lines()
                .any(|line| line.starts_with("effort = \"low\"  ")
                    && line.contains("# low | medium")),
            "{saved}"
        );
        assert!(
            saved.contains("effort = \"high\"                      # minimal"),
            "{saved}"
        );
        assert!(!saved.contains("\nsettings = "), "{saved}");
        assert!(
            saved
                .lines()
                .any(|line| line.starts_with("editor = \"\"  ") && line.contains("# lineup")),
            "{saved}"
        );
        let removed = changed.iter().find(|c| c.key == "xtc.settings").unwrap();
        assert_eq!(
            removed.old.as_deref(),
            Some("\"/etc/daily-epub/xtc-settings.json\"")
        );
        assert_eq!(removed.new, None);
        let config = state.config();
        assert_eq!(config.curation.blocked_domains, ["a.example", "b.example"]);
        assert_eq!(config.providers["anthropic"].effort.as_deref(), Some("low"));
        assert_eq!(config.xtc.settings, None);
        assert_eq!(config.llm.editor_name(), None);

        // A key the defaults fill in cannot be unset through the file.
        let outcome = save_settings(&state, admin, &posted(&[("providers.gemini.effort", "")]))
            .await
            .unwrap();
        let SaveOutcome::Rejected(errors) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            errors[0].contains("cannot be unset") || errors[0].contains("not unset"),
            "{errors:?}"
        );
        assert_eq!(
            state.config().providers["gemini"].effort.as_deref(),
            Some("high")
        );
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_and_the_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let (_db_dir, state, admin) = test_state(Some(&path)).await;
        let original = std::fs::read_to_string(&path).unwrap();

        // Field errors are collected before anything is written.
        let outcome = save_settings(
            &state,
            admin,
            &posted(&[
                ("lookback_hours", "thirty"),
                ("xtc.format", "pdf"),
                ("retention_days", "30"),
            ]),
        )
        .await
        .unwrap();
        let SaveOutcome::Rejected(errors) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("lookback_hours:"), "{errors:?}");
        assert!(
            errors[1].starts_with("xtc.format: must be one of"),
            "{errors:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        // A value the loader rejects leaves the file alone too.
        let outcome = save_settings(
            &state,
            admin,
            &posted(&[("curation.ranking.deep_keep", "10")]),
        )
        .await
        .unwrap();
        let SaveOutcome::Rejected(errors) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            errors[0].contains("deep_keep >= shortlist_keep"),
            "{errors:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(state.config().curation.ranking.deep_keep, 120);
        let (_, pagination) = config_changes(&state, 1).await.unwrap();
        assert_eq!(pagination.total, 0);

        // A good save keeps the permissions.
        let outcome = save_settings(&state, admin, &posted(&[("retention_days", "30")]))
            .await
            .unwrap();
        assert!(matches!(outcome, SaveOutcome::Saved { .. }), "{outcome:?}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[tokio::test]
    async fn providers_can_be_added_and_removed_unless_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let (_db_dir, state, admin) = test_state(Some(&path)).await;

        let refused = remove_provider(&state, admin, "anthropic").await.unwrap();
        let SaveOutcome::Rejected(errors) = refused else {
            panic!("{refused:?}");
        };
        assert!(errors[0].contains("llm.editor"), "{errors:?}");

        for bad in ["Bad-Name", "", "deepseek"] {
            let outcome = add_provider(&state, admin, bad, ProviderKind::OpenAi)
                .await
                .unwrap();
            assert!(
                matches!(outcome, SaveOutcome::Rejected(_)),
                "{bad}: {outcome:?}"
            );
        }

        let added = add_provider(&state, admin, "local_llm", ProviderKind::Anthropic)
            .await
            .unwrap();
        let SaveOutcome::Saved { changed, .. } = added else {
            panic!("{added:?}");
        };
        assert_eq!(changed[0].key, "providers.local_llm");
        assert!(
            changed[0]
                .new
                .as_deref()
                .unwrap()
                .starts_with("[providers.local_llm]\nkind = \"anthropic\"")
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains(
                "\n[providers.local_llm]\nkind = \"anthropic\"\nbase_url = \"https://api.example.com/v1\"\nmodel = \"model-name\"\n"
            ),
            "{saved}"
        );
        assert!(!saved.contains("api_key ="), "{saved}");
        assert_eq!(
            state.config().providers["local_llm"].kind,
            ProviderKind::Anthropic
        );
        let groups = schema(&state.config(), None);
        assert_eq!(
            field(&groups, "llm.bulk").kind,
            FieldKind::Enum(strings(&["anthropic", "deepseek", "gemini", "local_llm"]))
        );

        // Config accepts a hand-written provider key even though the docs say
        // it belongs in the environment. Removing that provider must not copy
        // the key into `config_changes` and expose it on the history page.
        let with_file_secret = saved.replace(
            "model = \"model-name\"\n",
            "model = \"model-name\"\napi_key = \"history-must-not-leak\"\n",
        );
        std::fs::write(&path, with_file_secret).unwrap();

        // Shipped providers come back from `Config::default()` however the
        // file looks, so they are refused rather than silently reset.
        let built_in = remove_provider(&state, admin, "gemini").await.unwrap();
        let SaveOutcome::Rejected(errors) = built_in else {
            panic!("{built_in:?}");
        };
        assert!(errors[0].contains("built in"), "{errors:?}");
        assert!(
            !groups
                .iter()
                .find(|group| group.id == "providers.gemini")
                .unwrap()
                .provider
                .as_ref()
                .unwrap()
                .removable()
        );

        let removed = remove_provider(&state, admin, "local_llm").await.unwrap();
        let SaveOutcome::Saved { changed, .. } = removed else {
            panic!("{removed:?}");
        };
        assert_eq!(changed[0].key, "providers.local_llm");
        assert!(
            changed[0]
                .old
                .as_deref()
                .unwrap()
                .contains("model = \"model-name\"")
        );
        assert!(
            !changed[0]
                .old
                .as_deref()
                .unwrap()
                .contains("history-must-not-leak")
        );
        assert_eq!(changed[0].new, None);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("[providers.local_llm]"), "{saved}");
        assert!(saved.contains("[providers.gemini]"), "{saved}");
        assert!(!state.config().providers.contains_key("local_llm"));
        let (changes, pagination) = config_changes(&state, 1).await.unwrap();
        assert_eq!(pagination.total, 2);
        assert_eq!(changes[0].key, "providers.local_llm");
        assert_eq!(changes[0].new_value, None);
        assert!(
            !changes[0]
                .old_value
                .as_deref()
                .unwrap()
                .contains("history-must-not-leak")
        );
        assert_eq!(changes[1].key, "providers.local_llm");
        assert_eq!(changes[1].old_value, None);
    }

    #[tokio::test]
    async fn reload_on_mtime_swaps_the_config_and_reports_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let (_db_dir, state, _) = test_state(Some(&path)).await;
        assert!(WebState::reload_if_changed(&state).unwrap());
        assert!(!WebState::reload_if_changed(&state).unwrap());

        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("lookback_hours = 26", "lookback_hours = 40");
        std::fs::write(&path, edited).unwrap();
        touch_later(&path, 5);
        assert!(WebState::reload_if_changed(&state).unwrap());
        assert_eq!(state.config().lookback_hours, 40);
        assert!(!WebState::reload_if_changed(&state).unwrap());

        std::fs::write(&path, "lookback_hours = 0\n").unwrap();
        touch_later(&path, 10);
        let error = WebState::reload_if_changed(&state).unwrap_err();
        assert!(error.to_string().contains("lookback_hours"), "{error}");
        assert_eq!(state.config().lookback_hours, 40);

        let (_db_dir, none, _) = test_state(None).await;
        assert!(!WebState::reload_if_changed(&none).unwrap());
    }

    async fn login(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.50")
                    .body(Body::from(
                        "username=admin&password=correct+horse+battery&next=%2F",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn post(uri: &str, cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn settings_pages_render_save_and_show_hand_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let with_secret = std::fs::read_to_string(&path).unwrap().replace(
            "# opds_pass: environment only (DAILY_EPUB_BOOKORBIT__OPDS_PASS)",
            "opds_pass = \"hunter2-bookorbit\"",
        );
        std::fs::write(&path, with_secret).unwrap();
        let (_db_dir, state, _) = test_state(Some(&path)).await;
        let app = router(state.clone());
        let cookie = login(&app).await;

        let page = app
            .clone()
            .oneshot(get("/dashboard/settings", &cookie))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = text(page).await;
        for anchor in [
            "id=\"curation.feedback\"",
            "id=\"curation.ranking\"",
            "id=\"curation.ranking.weights.preliminary\"",
            "id=\"providers.gemini\"",
            "id=\"bookorbit\"",
        ] {
            assert!(html.contains(anchor), "{anchor}");
        }
        assert!(html.contains("name=\"curation.ranking.deep_keep\""));
        assert!(html.contains("renormalized"));
        assert!(html.contains("not set"));
        assert!(html.contains("in the config file — move it to the env file"));
        assert!(!html.contains("hunter2-bookorbit"));
        assert!(html.contains("/dashboard/settings/history"));

        let saved = app
            .clone()
            .oneshot(post(
                "/dashboard/settings",
                &cookie,
                "lookback_hours=28&server.bind=127.0.0.1%3A3600",
            ))
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            saved.headers().get(header::LOCATION).unwrap(),
            "/dashboard/settings"
        );
        let page = text(
            app.clone()
                .oneshot(get("/dashboard/settings", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(page.contains("Saved 2 settings"), "{page}");
        assert!(
            page.contains("Restart the server for: server.bind"),
            "{page}"
        );
        assert!(page.contains("value=\"28\""));
        assert_eq!(state.config().lookback_hours, 28);

        let rejected = app
            .clone()
            .oneshot(post("/dashboard/settings", &cookie, "lookback_hours=abc"))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let html = text(rejected).await;
        assert!(
            html.contains("lookback_hours: must be a whole number"),
            "{html}"
        );
        assert!(html.contains("value=\"abc\""), "{html}");

        // A hand edit on disk shows up on the next view; a broken one is reported.
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("lookback_hours = 28", "lookback_hours = 31");
        std::fs::write(&path, edited).unwrap();
        touch_later(&path, 5);
        let page = text(
            app.clone()
                .oneshot(get("/dashboard/settings", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(page.contains("value=\"31\""), "{page}");
        std::fs::write(&path, "lookback_hours = 0\n").unwrap();
        touch_later(&path, 10);
        let page = text(
            app.clone()
                .oneshot(get("/dashboard/settings", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            page.contains("config.toml on disk does not load:"),
            "{page}"
        );
        assert!(page.contains("value=\"31\""), "{page}");

        let history = text(
            app.clone()
                .oneshot(get("/dashboard/settings/history", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(history.contains("server.bind"), "{history}");
        assert!(history.contains("127.0.0.1:3600"), "{history}");
        assert!(history.contains("admin"), "{history}");
    }

    #[tokio::test]
    async fn provider_forms_and_missing_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = copy_example(dir.path());
        let (_db_dir, state, _) = test_state(Some(&path)).await;
        let app = router(state.clone());
        let cookie = login(&app).await;
        let added = app
            .clone()
            .oneshot(post(
                "/dashboard/settings/providers",
                &cookie,
                "action=add&name=mistral&kind=openai",
            ))
            .await
            .unwrap();
        assert_eq!(added.status(), StatusCode::SEE_OTHER);
        assert!(state.config().providers.contains_key("mistral"));
        let refused = app
            .clone()
            .oneshot(post(
                "/dashboard/settings/providers",
                &cookie,
                "action=remove&name=deepseek",
            ))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert!(text(refused).await.contains("llm.bulk"));
        let bad_kind = app
            .clone()
            .oneshot(post(
                "/dashboard/settings/providers",
                &cookie,
                "action=add&name=other&kind=cohere",
            ))
            .await
            .unwrap();
        assert_eq!(bad_kind.status(), StatusCode::BAD_REQUEST);

        let (_dir, no_path, _) = test_state(None).await;
        let app = router(no_path);
        let cookie = login(&app).await;
        let page = app
            .clone()
            .oneshot(get("/dashboard/settings", &cookie))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert!(text(page).await.contains("No config file is configured"));
        let save = app
            .clone()
            .oneshot(post("/dashboard/settings", &cookie, "lookback_hours=28"))
            .await
            .unwrap();
        assert_eq!(save.status(), StatusCode::BAD_REQUEST);
        let anonymous = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::FOUND);
    }
}
