//! The operator dashboard (`/dashboard/*`, dashboard plan §9–§14).
//!
//! Every route here sits under the admin `permission_required!` layer that
//! `web::router` applies to the merged router; handlers can therefore trust
//! that `AuthSession::user()` is an admin. One submodule per page group; each
//! exposes `routes()` and this module merges them.
//!
//! This file also carries the small helpers the read-only pages share: the
//! pager, number/time formatting, the `signals_json` view behind
//! `_signals_table.html`, the allow-list check and the LIKE-pattern escaper.

pub mod articles;
pub mod jobs;
pub mod profile;
pub mod ratings;
pub mod runs;
pub mod settings;
pub mod stats;
pub mod users;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use sqlx::Row as _;

use crate::config::Config;
use crate::curate::telemetry::SignalsJson;
use crate::db::Db;
use crate::report::RunReport;
use crate::server::AppState;
use crate::types::ArticleId;
use crate::web::rate::RatingWidget;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, Pagination, WebError, encode_component, format_time, take_flash};

/// Every dashboard route, without the admin layer (applied by the caller).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(overview))
        .merge(runs::routes())
        .merge(articles::routes())
        .merge(ratings::routes())
        .merge(profile::routes())
        .merge(settings::routes())
        .merge(jobs::routes())
        .merge(stats::routes())
        .merge(users::routes())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Signal names in the order of curation plan §7.5.
pub const SIGNAL_NAMES: [&str; 8] = [
    "interest",
    "knn",
    "feed",
    "social",
    "heuristic",
    "triage",
    "quality",
    "fit",
];

/// `candidate_runs.stage` values in pipeline order (curation plan §7.4).
pub const STAGES: [&str; 7] = [
    "excluded",
    "eligible",
    "triaged",
    "admitted",
    "assessed",
    "shortlisted",
    "selected",
];

/// `candidate_runs.excluded_reason` values (curation plan §7.4).
pub const REASONS: [&str; 8] = [
    "blocked",
    "published_before",
    "recently_rejected",
    "not_admitted",
    "cluster_suppressed",
    "shortlist_cap",
    "not_selected",
    "over_max",
];

/// Retriever names that can head `admitted_by` (curation plan §11).
pub const RETRIEVERS: [&str; 6] = [
    "auto_include",
    "triage",
    "interest",
    "knn",
    "exploration",
    "blend",
];

/// A raw `sqlx` error as the dashboard's error type.
pub(crate) fn db_err(error: sqlx::Error) -> WebError {
    WebError::Db(error.into())
}

/// `?page=N`, clamped to at least 1.
pub fn page_number(raw: Option<u32>) -> u32 {
    raw.unwrap_or(1).max(1)
}

/// Keep a query value only when it is one of `allowed`; anything else is
/// dropped (never an error, never interpolated).
pub fn allow_listed<'a>(value: Option<&'a str>, allowed: &[&str]) -> Option<&'a str> {
    value.filter(|value| allowed.contains(value))
}

/// A non-empty, trimmed query value.
pub fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// `%…%` with the LIKE metacharacters escaped, for `LIKE ? ESCAPE '\'`.
pub fn like_pattern(needle: &str) -> String {
    let mut pattern = String::with_capacity(needle.len() + 2);
    pattern.push('%');
    for ch in needle.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// A bound query parameter for the dynamically assembled list queries. The
/// SQL text only ever contains `?` placeholders and allow-listed fragments.
#[derive(Debug, Clone)]
pub enum Bind {
    Text(String),
    Int(i64),
}

pub type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

/// A query assembled at runtime. Audited: every caller builds `sql` from
/// string constants and allow-listed fragments only, and passes user values
/// through [`bind_all`] as bound parameters.
pub fn dynamic_query<'q>(sql: String) -> SqliteQuery<'q> {
    sqlx::query(sqlx::AssertSqlSafe(sql))
}

pub fn bind_all<'q>(mut query: SqliteQuery<'q>, values: &[Bind]) -> SqliteQuery<'q> {
    for value in values {
        query = match value {
            Bind::Text(text) => query.bind(text.clone()),
            Bind::Int(number) => query.bind(*number),
        };
    }
    query
}

/// Prev/next links for a paginated table. `path` plus every query parameter
/// except `page` is preserved.
#[derive(Debug, Clone)]
pub struct Pager {
    pub page: u32,
    pub pages: u32,
    pub total: i64,
    pub prev_href: Option<String>,
    pub next_href: Option<String>,
}

impl Pager {
    pub fn new(pagination: Pagination, path: &str, params: &[(&str, Option<String>)]) -> Self {
        let query: Vec<String> = params
            .iter()
            .filter_map(|(key, value)| {
                value
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("{key}={}", encode_component(value)))
            })
            .collect();
        let href = |page: u32| {
            let mut parts = query.clone();
            parts.push(format!("page={page}"));
            format!("{path}?{}", parts.join("&"))
        };
        let pages = pagination.pages();
        Self {
            page: pagination.page,
            pages,
            total: pagination.total,
            prev_href: (pagination.page > 1).then(|| href(pagination.page - 1)),
            next_href: (pagination.page < pages).then(|| href(pagination.page + 1)),
        }
    }
}

pub fn fmt_usd(value: f64) -> String {
    format!("${value:.2}")
}

pub fn fmt_opt(value: Option<f64>, decimals: usize) -> String {
    value
        .map(|value| format!("{value:.decimals$}"))
        .unwrap_or_else(|| "—".into())
}

pub fn fmt_opt_int(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
}

/// A stored RFC3339 timestamp rendered in the configured zone; a malformed
/// value is shown as stored rather than hidden.
pub fn fmt_stored_time(raw: Option<&str>, config: &Config) -> String {
    match raw {
        Some(raw) => raw
            .parse::<Timestamp>()
            .map(|timestamp| format_time(timestamp, config))
            .unwrap_or_else(|_| raw.to_string()),
        None => "—".into(),
    }
}

pub fn duration_between(started: &str, finished: Option<&str>) -> Option<i64> {
    let started = started.parse::<Timestamp>().ok()?;
    let finished = finished?.parse::<Timestamp>().ok()?;
    Some((finished.as_second() - started.as_second()).max(0))
}

pub fn fmt_duration(secs: Option<i64>) -> String {
    secs.map(RunReport::format_duration)
        .unwrap_or_else(|| "—".into())
}

/// `admitted_by[0]` plus the rest, from the stored JSON array.
pub fn admitted_by_parts(raw: Option<&str>) -> (Option<String>, Vec<String>) {
    let mut names = raw
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .unwrap_or_default()
        .into_iter();
    let first = names.next();
    (first, names.collect())
}

/// The widget's label for a stored `rating_events.label`.
pub fn widget_label(label: Option<&str>) -> &'static str {
    match label {
        Some("not_for_me" | "down") => "down",
        Some("loved") => "loved",
        Some("good") => "good",
        Some("cleared") => "cleared",
        _ => "",
    }
}

/// One line of `_signals_table.html`.
#[derive(Debug, Clone)]
pub struct SignalLine {
    pub name: &'static str,
    pub raw: String,
    pub norm: String,
    pub weight: String,
    pub present: bool,
}

#[derive(Debug, Clone)]
pub struct InterestLine {
    pub name: String,
    pub z: String,
    pub cos: String,
}

#[derive(Debug, Clone)]
pub struct NeighbourLine {
    pub article_id: ArticleId,
    pub label: String,
    pub cos: String,
    pub title: String,
}

/// The parsed `signals_json` of one row, shaped for the partial.
#[derive(Debug, Clone, Default)]
pub struct SignalsView {
    pub lines: Vec<SignalLine>,
    pub blend: String,
    pub top1_cos: Option<String>,
    pub top_interests: Vec<InterestLine>,
    pub neighbours: Vec<NeighbourLine>,
    pub exploration: bool,
    pub auto_include: bool,
    pub notes: Vec<String>,
    /// A thin hygiene row (`{}`) or unparseable JSON: nothing to show.
    pub empty: bool,
}

impl SignalsView {
    pub fn from_json(raw: &str) -> Self {
        let Some(signals) = serde_json::from_str::<SignalsJson>(raw).ok() else {
            return Self {
                empty: true,
                ..Self::default()
            };
        };
        Self::from_signals(&signals)
    }

    pub fn from_signals(signals: &SignalsJson) -> Self {
        let lines = SIGNAL_NAMES
            .into_iter()
            .map(|name| SignalLine {
                name,
                raw: fmt_opt(signals.raw.get(name).copied(), 3),
                norm: fmt_opt(signals.norm.get(name).copied(), 3),
                weight: fmt_opt(signals.weights.get(name).copied(), 3),
                present: signals.present.get(name).copied().unwrap_or(false),
            })
            .collect();
        let empty = signals.present.is_empty()
            && signals.raw.is_empty()
            && signals.top_interests.is_empty()
            && signals.neighbours.is_empty()
            && signals.notes.is_empty();
        Self {
            lines,
            blend: fmt_opt(signals.blend(), 1),
            top1_cos: signals
                .raw
                .get("interest_top1_cos")
                .map(|cos| format!("{cos:.3}")),
            top_interests: signals
                .top_interests
                .iter()
                .map(|interest| InterestLine {
                    name: interest.name.clone(),
                    z: format!("{:.2}", interest.z),
                    cos: format!("{:.3}", interest.cos),
                })
                .collect(),
            neighbours: signals
                .neighbours
                .iter()
                .map(|neighbour| NeighbourLine {
                    article_id: neighbour.article_id,
                    label: neighbour.label.clone(),
                    cos: format!("{:.3}", neighbour.cos),
                    title: neighbour.title.clone(),
                })
                .collect(),
            exploration: signals.exploration,
            auto_include: signals.auto_include,
            notes: signals.notes.clone(),
            empty,
        }
    }
}

// ---------------------------------------------------------------------------
// Overview (§9.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LastRunCard {
    id: i64,
    date: String,
    status: String,
    started: String,
    duration: String,
    lines: Vec<String>,
    warnings: usize,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct BudgetLine {
    provider: String,
    spent: String,
    ceiling: String,
    /// `<meter>` value and max; `max` is 1 when there is no ceiling.
    value: f64,
    max: f64,
    over: bool,
}

#[derive(Debug, Clone)]
struct LabelCount {
    label: String,
    count: i64,
}

#[derive(Debug, Clone)]
struct UnratedPick {
    title: String,
    feed: String,
    issue_href: String,
    issue_date: String,
    widget: RatingWidget,
}

#[derive(Debug, Clone)]
struct JobLine {
    id: i64,
    name: String,
    status: String,
    requested: String,
    finished: String,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
struct OverviewTemplate {
    page: Page,
    last_run: Option<LastRunCard>,
    budget: Vec<BudgetLine>,
    ratings: Vec<LabelCount>,
    ratings_total: i64,
    unrated: Vec<UnratedPick>,
    active_jobs: Vec<JobLine>,
    finished_jobs: Vec<JobLine>,
    config_warnings: Vec<String>,
    sparklines: Vec<stats::Sparkline>,
}

/// `GET /dashboard` — the overview (§9.1).
async fn overview(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let now = Timestamp::now();
    let db = &state.db;

    let last_run = last_run_card(db, &config).await?;
    let budget = budget_lines(db, &config, now).await?;
    let (ratings, ratings_total) = ratings_this_week(db, now).await?;
    let unrated = unrated_picks(db).await?;
    let (active_jobs, finished_jobs) = jobs_summary(db, &config).await?;
    let sparklines = overview_sparklines(db).await?;
    let config_warnings = config
        .check_report(state.config_path.as_deref())
        .into_iter()
        .filter(|line| line.starts_with("! "))
        .collect();

    let mut page = Page::new("Overview", viewer, "dashboard");
    page.flash = take_flash(&session).await?;
    Ok(Html(OverviewTemplate {
        page,
        last_run,
        budget,
        ratings,
        ratings_total,
        unrated,
        active_jobs,
        finished_jobs,
        config_warnings,
        sparklines,
    })
    .into_response())
}

/// How many finished non-dry runs the overview sparklines cover (§9.1).
const SPARKLINE_RUNS: i64 = 30;

/// Cost per run, selected per run and generation seconds over the last 30
/// non-dry runs (§9.1), drawn by `dashboard/_sparkline.html`.
async fn overview_sparklines(db: &Db) -> Result<Vec<stats::Sparkline>, WebError> {
    let runs = crate::curate::telemetry::run_series(db, None, Some(SPARKLINE_RUNS))
        .await
        .map_err(db_err)?;
    let labels = (
        runs.first().map(|run| run.date.as_str()).unwrap_or(""),
        runs.last().map(|run| run.date.as_str()).unwrap_or(""),
    );
    let costs: Vec<f64> = runs.iter().map(|run| run.cost_usd).collect();
    let selected: Vec<f64> = runs.iter().map(|run| run.selected as f64).collect();
    let seconds: Vec<f64> = runs
        .iter()
        .map(|run| run.duration_secs.unwrap_or(0) as f64)
        .collect();
    Ok(vec![
        stats::Sparkline::line("Cost per run", &costs, labels, "runs", fmt_usd),
        stats::Sparkline::line("Selected per run", &selected, labels, "runs", |n| {
            format!("{n:.0}")
        }),
        stats::Sparkline::line("Generation time", &seconds, labels, "runs", |secs| {
            RunReport::format_duration(secs as i64)
        }),
    ])
}

async fn last_run_card(db: &Db, config: &Config) -> Result<Option<LastRunCard>, WebError> {
    let Some(row) = sqlx::query(
        "SELECT id, date, status, started_at, finished_at, entries_fetched, candidates,
                selected, cost_usd, error, report_json
         FROM runs ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?
    else {
        return Ok(None);
    };
    let started_at: String = row.get("started_at");
    let finished_at: Option<String> = row.get("finished_at");
    let duration = duration_between(&started_at, finished_at.as_deref());
    let report = row
        .get::<Option<String>, _>("report_json")
        .and_then(|raw| serde_json::from_str::<RunReport>(&raw).ok());
    let (lines, warnings) = match &report {
        Some(report) => (report.info_block().to_vec(), report.warnings.len()),
        None => (
            vec![
                format!(
                    "curation: {} entries → {} candidates → {} selected",
                    row.get::<i64, _>("entries_fetched"),
                    row.get::<i64, _>("candidates"),
                    row.get::<i64, _>("selected")
                ),
                format!(
                    "providers: total {} · {}",
                    fmt_usd(row.get::<f64, _>("cost_usd")),
                    fmt_duration(duration)
                ),
            ],
            0,
        ),
    };
    Ok(Some(LastRunCard {
        id: row.get("id"),
        date: row.get("date"),
        status: row.get("status"),
        started: fmt_stored_time(Some(&started_at), config),
        duration: fmt_duration(duration),
        lines,
        warnings,
        error: row.get("error"),
    }))
}

async fn budget_lines(
    db: &Db,
    config: &Config,
    now: Timestamp,
) -> Result<Vec<BudgetLine>, WebError> {
    // Every run that started today (UTC) began before `now`, so this already
    // includes the last run when it ran today.
    let spent = db.provider_spend_for_utc_day(now).await?;
    let mut providers: Vec<(String, f64)> = config
        .referenced_providers()
        .into_iter()
        .map(|(name, provider)| (name.to_string(), provider.max_daily_usd))
        .collect();
    if config.voyage.enabled {
        providers.push((
            crate::report::VOYAGE_PROVIDER.to_string(),
            config.voyage.max_daily_usd,
        ));
    }
    Ok(providers
        .into_iter()
        .map(|(name, ceiling)| {
            let used = spent.get(&name).copied().unwrap_or(0.0);
            BudgetLine {
                provider: name,
                spent: fmt_usd(used),
                ceiling: if ceiling > 0.0 {
                    fmt_usd(ceiling)
                } else {
                    "no ceiling".into()
                },
                value: used.max(0.0),
                max: if ceiling > 0.0 { ceiling } else { 1.0 },
                over: ceiling > 0.0 && used >= ceiling,
            }
        })
        .collect())
}

async fn ratings_this_week(db: &Db, now: Timestamp) -> Result<(Vec<LabelCount>, i64), WebError> {
    let since = now
        .checked_sub(jiff::Span::new().hours(7 * 24))
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let rows = sqlx::query(
        "SELECT label, COUNT(*) AS n FROM rating_events
         WHERE kind = 'explicit' AND event_at >= ? GROUP BY label ORDER BY label",
    )
    .bind(crate::db::fmt_ts(since))
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let counts: Vec<LabelCount> = rows
        .iter()
        .map(|row| LabelCount {
            label: row.get("label"),
            count: row.get("n"),
        })
        .collect();
    let total = counts
        .iter()
        .filter(|count| count.label != "cleared")
        .map(|count| count.count)
        .sum();
    Ok((counts, total))
}

async fn unrated_picks(db: &Db) -> Result<Vec<UnratedPick>, WebError> {
    let rows = sqlx::query(
        "SELECT ia.issue_date, ia.article_id, COALESCE(a.title, '') AS title,
                COALESCE(e.feed_title, '') AS feed_title
         FROM issue_articles ia
         JOIN articles a ON a.id = ia.article_id
         LEFT JOIN entries e ON e.id = a.best_entry_id
         WHERE ia.issue_date IN (SELECT date FROM issues ORDER BY date DESC LIMIT 3)
           AND NOT EXISTS (SELECT 1 FROM rating_events re
                           WHERE re.article_id = ia.article_id AND re.kind = 'explicit')
         ORDER BY ia.issue_date DESC, ia.section, ia.position",
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok(rows
        .iter()
        .map(|row| {
            let issue_date: String = row.get("issue_date");
            let article_id: ArticleId = row.get("article_id");
            UnratedPick {
                title: row.get("title"),
                feed: row.get("feed_title"),
                issue_href: format!("/issues/{issue_date}/articles/{article_id}"),
                widget: RatingWidget {
                    article_id,
                    issue_date: issue_date.clone(),
                    next: "/dashboard".into(),
                    current: String::new(),
                    show_note: false,
                },
                issue_date,
            }
        })
        .collect())
}

async fn jobs_summary(db: &Db, config: &Config) -> Result<(Vec<JobLine>, Vec<JobLine>), WebError> {
    let job_line = |row: &sqlx::sqlite::SqliteRow| JobLine {
        id: row.get("id"),
        name: row.get("name"),
        status: row.get("status"),
        requested: fmt_stored_time(
            row.get::<Option<String>, _>("requested_at").as_deref(),
            config,
        ),
        finished: fmt_stored_time(
            row.get::<Option<String>, _>("finished_at").as_deref(),
            config,
        ),
        message: row.get("message"),
    };
    let active = sqlx::query(
        "SELECT id, name, status, requested_at, finished_at, message FROM jobs
         WHERE status IN ('requested', 'running') ORDER BY id DESC",
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let finished = sqlx::query(
        "SELECT id, name, status, requested_at, finished_at, message FROM jobs
         WHERE status IN ('ok', 'failed') ORDER BY id DESC LIMIT 5",
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok((
        active.iter().map(job_line).collect(),
        finished.iter().map(job_line).collect(),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use jiff::civil::Date;
    use tower::ServiceExt;

    use super::*;
    use crate::curate::telemetry::{self, CandidateRun};
    use crate::server::router;
    use crate::types::Entry;

    /// A temp database with eight articles over two feeds, an earlier run,
    /// one finished run (`run_id`) with a full funnel, assessments, an
    /// embedding, one issue and two rating events on article 1.
    pub(crate) struct Seed {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) db: Db,
        pub(crate) run_id: i64,
        pub(crate) earlier_run_id: i64,
        pub(crate) date: Date,
    }

    pub(crate) async fn seed() -> Seed {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        for id in 1..=8i64 {
            let feed_id = if id % 2 == 0 { 20 } else { 10 };
            db.upsert_entry(&Entry {
                id: 100 + id,
                feed_id,
                feed_title: Some(if feed_id == 10 {
                    "Alpha Blog".into()
                } else {
                    "Beta Weekly".into()
                }),
                category: Some("Tech".into()),
                title: format!("Article {id}"),
                url: format!("https://example.com/{id}"),
                canonical_url: Some(format!("https://example.com/{id}")),
                author: Some("Ada".into()),
                published_at: Some("2026-09-01T08:00:00Z".parse().unwrap()),
                comments_url: None,
                raw_content: "<p>Body</p>".into(),
                fetched_at: "2026-09-02T04:00:00Z".parse().unwrap(),
            })
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO articles (id, canonical_url, title, best_entry_id, content_html,
                                       word_count, first_seen)
                 VALUES (?, ?, ?, ?, '<p>Body</p>', ?, ?)",
            )
            .bind(id)
            .bind(format!("https://example.com/{id}"))
            .bind(format!(
                "Article {id} about {}",
                if id % 2 == 0 { "graphs" } else { "prose" }
            ))
            .bind(100 + id)
            .bind(300 * id)
            .bind(format!("2026-09-0{}T04:00:00Z", (id % 3) + 1))
            .execute(db.pool())
            .await
            .unwrap();
        }
        let date: Date = "2026-09-02".parse().unwrap();
        let earlier_run_id = db
            .start_run(date, "2026-09-01T05:30:00Z".parse().unwrap())
            .await
            .unwrap();
        let mut earlier = RunReport::new(date, "2026-09-01T05:30:00Z".parse().unwrap());
        earlier.config_json = serde_json::json!({
            "curation": {"deep_keep": 100, "shortlist_keep": 60, "sections": ["A", "B"]},
            "llm": {"bulk": "deepseek"}
        });
        earlier.finish("2026-09-01T05:40:00Z".parse().unwrap());
        db.finish_run(earlier_run_id, &earlier).await.unwrap();

        let run_id = db
            .start_run(date, "2026-09-02T05:30:00Z".parse().unwrap())
            .await
            .unwrap();
        let mut report = RunReport::new(date, "2026-09-02T05:30:00Z".parse().unwrap());
        report.counts.articles = 8;
        report.counts.eligible = 6;
        report.counts.triaged = 6;
        report.counts.assessed = 4;
        report.counts.shortlisted = 3;
        report.counts.selected = 2;
        report.counts.rated_with_embeddings = 14;
        report.counts.knn_gate = 0.35;
        report.counts.verdicts_in_prompt = 41;
        report.counts.admitted_by.insert("triage".into(), 3);
        report.counts.admitted_by.insert("blend".into(), 1);
        report.timings.record("triage", 12_000);
        report.timings.record("editor", 30_000);
        report.per_feed_counts.insert("Alpha Blog".into(), 4);
        report.per_feed_counts.insert("Beta Weekly".into(), 4);
        report.provider_costs.insert(
            "deepseek".into(),
            crate::report::ProviderUsage {
                usage: crate::types::TokenUsage {
                    input_tokens: 1000,
                    cached_tokens: 200,
                    cache_write_tokens: 0,
                    output_tokens: 300,
                },
                cost_usd: 0.11,
            },
        );
        report.warn("social: lobsters lookup timed out");
        report.config_json = serde_json::json!({
            "curation": {"deep_keep": 120, "shortlist_keep": 60, "sections": ["A", "B"]},
            "llm": {"bulk": "deepseek", "editor": "anthropic"}
        });
        report.finish("2026-09-02T05:53:12Z".parse().unwrap());
        db.finish_run(run_id, &report).await.unwrap();

        let signals = |quality: f64, exploration: bool| {
            serde_json::json!({
                "v": 1,
                "raw": {"interest": 1.2, "triage": 7.0, "quality": quality, "fit": 6.0},
                "norm": {"interest": 0.9, "triage": 0.7, "quality": quality / 10.0, "fit": 0.6},
                "present": {"interest": true, "knn": false, "feed": false, "social": false,
                            "heuristic": false, "triage": true, "quality": true, "fit": true},
                "weights": {"interest": 0.2, "triage": 0.1, "quality": 0.5, "fit": 0.2},
                "top_interests": [{"name": "Gaussian Splatting", "z": 3.4, "cos": 0.61}],
                "neighbours": [{"article_id": 3, "label": "loved", "cos": 0.71, "title": "Article 3"}],
                "exploration": exploration,
                "auto_include": false,
                "notes": ["knn gate 0.35 (n=14 rated with embeddings)"]
            })
            .to_string()
        };
        // 1 selected · 2 selected (exploration) · 3 shortlisted (not_selected)
        // · 4 assessed (cluster_suppressed) · 5, 6 triaged (not_admitted)
        // · 7 excluded (blocked) · 8 excluded (published_before)
        let rows = [
            (
                1,
                "selected",
                None,
                Some("[\"triage\",\"interest\"]"),
                88.0,
                1,
                Some("Why one"),
            ),
            (
                2,
                "selected",
                None,
                Some("[\"blend\"]"),
                80.0,
                2,
                Some("Why two"),
            ),
            (
                3,
                "shortlisted",
                Some("not_selected"),
                Some("[\"triage\"]"),
                75.0,
                3,
                None,
            ),
            (
                4,
                "assessed",
                Some("cluster_suppressed"),
                Some("[\"triage\"]"),
                60.0,
                4,
                None,
            ),
        ];
        for (article_id, stage, reason, admitted_by, utility, rank, why) in rows {
            let json = signals(utility / 10.0, article_id == 2);
            telemetry::write(
                &db,
                &CandidateRun {
                    run_id,
                    article_id,
                    stage,
                    excluded_reason: reason,
                    admitted_by,
                    signals_json: &json,
                    utility: Some(utility),
                    rank_utility: Some(rank),
                    cluster_id: Some(1),
                    cluster_rank: Some(rank),
                    editor_why: why,
                },
            )
            .await
            .unwrap();
        }
        for article_id in [5, 6] {
            let json = signals(0.0, false);
            telemetry::write(
                &db,
                &CandidateRun {
                    run_id,
                    article_id,
                    stage: "triaged",
                    excluded_reason: Some("not_admitted"),
                    admitted_by: None,
                    signals_json: &json,
                    utility: None,
                    rank_utility: None,
                    cluster_id: None,
                    cluster_rank: None,
                    editor_why: None,
                },
            )
            .await
            .unwrap();
        }
        telemetry::thin_excluded(&db, run_id, 7, "blocked")
            .await
            .unwrap();
        telemetry::thin_excluded(&db, run_id, 8, "published_before")
            .await
            .unwrap();
        // Article 1 was also seen by the earlier run; its latest row is the
        // newer one.
        telemetry::write(
            &db,
            &CandidateRun {
                run_id: earlier_run_id,
                article_id: 1,
                stage: "triaged",
                excluded_reason: Some("not_admitted"),
                admitted_by: None,
                signals_json: "{}",
                utility: None,
                rank_utility: None,
                cluster_id: None,
                cluster_rank: None,
                editor_why: None,
            },
        )
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO article_assessments
             (article_id, stage, model, prompt_version, score, fit, kind, facets_json, rationale,
              category, paywalled_guess, assessed_at)
             VALUES (1, 'triage', 'deepseek-v4-flash', 1, 7.0, NULL, 'essay', NULL,
                     'A specific argument', NULL, 0, '2026-09-02T05:31:00Z'),
                    (1, 'deep', 'deepseek-v4-flash', 1, 8.8, 6.0, 'analysis_essay',
                     '{\"depth\":\"deep\",\"topic_group\":\"ai_ml\"}',
                     'Careful and first-hand', 'Top Stories', 0, '2026-09-02T05:35:00Z'),
                    (2, 'triage', 'deepseek-v4-flash', 1, 6.0, NULL, 'news', NULL,
                     'Newsy', NULL, 0, '2026-09-02T05:31:00Z'),
                    (3, 'triage', 'deepseek-v4-flash', 1, NULL, NULL, 'provider_rejected', NULL,
                     'deepseek: 400 Bad Request: Content Exists Risk', NULL, 0,
                     '2026-09-02T05:31:00Z'),
                    (4, 'deep', 'deepseek-v4-flash', 1, 5.0, 4.0, 'reported_news', NULL,
                     'Fine', 'World', 1, '2026-09-02T05:35:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO article_embeddings
             (article_id, model, dimension, input_hash, embedding, created_at)
             VALUES (1, 'voyage-4-lite', 4, 'abc123', X'00000000000000000000000000000000',
                     '2026-09-02T05:30:30Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        db.upsert_issue(
            date,
            12,
            "2026-09-02T05:53:12Z".parse().unwrap(),
            None,
            None,
            None,
            Some("<p>Brief</p>"),
            Some(&report.to_json()),
            None,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issue_articles
             (issue_date, article_id, section, position, is_lead, summary, why)
             VALUES (?, 1, 'Top Stories', 0, 1, 'Summary one', 'Why one'),
                    (?, 2, 'Top Stories', 1, 0, 'Summary two', 'Why two')",
        )
        .bind(date.to_string())
        .bind(date.to_string())
        .execute(db.pool())
        .await
        .unwrap();
        for (label, value, at) in [
            ("good", 0.35, "2026-09-02T09:00:00Z"),
            ("loved", 1.0, "2026-09-02T10:00:00Z"),
        ] {
            db.append_rating_event(&crate::types::RatingEvent {
                id: 0,
                user_id: None,
                article_id: 1,
                issue_date: Some(date),
                kind: "explicit".into(),
                source: "cli".into(),
                label: label.into(),
                value,
                note: Some(format!("note {label}")),
                event_at: at.parse().unwrap(),
            })
            .await
            .unwrap();
        }
        Seed {
            _dir: dir,
            db,
            run_id,
            earlier_run_id,
            date,
        }
    }

    pub(crate) async fn login_cookie(app: &axum::Router, username: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.77")
                    .body(Body::from(format!(
                        "username={username}&password={password}&next=%2F"
                    )))
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

    pub(crate) async fn response_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    pub(crate) async fn get(app: &axum::Router, uri: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// Anonymous → 302 to login, `user` → 403, admin → 200; returns the
    /// admin's body.
    pub(crate) async fn assert_admin_only(app: &axum::Router, uri: &str) -> String {
        let anonymous = get(app, uri, None).await;
        assert_eq!(anonymous.status(), StatusCode::FOUND, "{uri}");
        assert!(
            anonymous
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("/login?next="),
            "{uri}"
        );
        let reader = login_cookie(app, "reader", "correct horse battery").await;
        let forbidden = get(app, uri, Some(&reader)).await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN, "{uri}");
        let admin = login_cookie(app, "admin", "correct horse battery").await;
        let allowed = get(app, uri, Some(&admin)).await;
        assert_eq!(allowed.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            allowed.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        response_text(allowed).await
    }

    pub(crate) async fn app_with_users(db: &Db) -> axum::Router {
        crate::web::users::add(db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        crate::web::users::add(db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        router(AppState::new(db.clone(), Config::default(), None))
    }

    #[test]
    fn like_patterns_escape_metacharacters() {
        assert_eq!(like_pattern("50% off_now\\"), "%50\\% off\\_now\\\\%");
        assert_eq!(like_pattern(""), "%%");
    }

    #[test]
    fn allow_list_drops_unknown_values() {
        assert_eq!(allow_listed(Some("selected"), &STAGES), Some("selected"));
        assert_eq!(allow_listed(Some("selected; DROP"), &STAGES), None);
        assert_eq!(allow_listed(None, &STAGES), None);
    }

    #[test]
    fn pager_keeps_other_parameters_and_omits_empty_ones() {
        let pager = Pager::new(
            Pagination {
                page: 2,
                per_page: 10,
                total: 25,
            },
            "/dashboard/articles",
            &[("q", Some("a b".into())), ("stage", None)],
        );
        assert_eq!(pager.pages, 3);
        assert_eq!(
            pager.prev_href.as_deref(),
            Some("/dashboard/articles?q=a+b&page=1")
        );
        assert_eq!(
            pager.next_href.as_deref(),
            Some("/dashboard/articles?q=a+b&page=3")
        );
    }

    #[test]
    fn signals_view_marks_absent_signals_and_flags() {
        let view = SignalsView::from_json(
            r#"{"v":1,"raw":{"interest":1.2,"interest_top1_cos":0.61},"norm":{"interest":0.9},
                "present":{"interest":true,"knn":false},"weights":{"interest":1.0},
                "exploration":true,"notes":["n"]}"#,
        );
        assert!(!view.empty);
        assert!(view.lines[0].present);
        assert_eq!(view.lines[0].raw, "1.200");
        assert!(!view.lines[1].present);
        assert_eq!(view.lines[1].raw, "—");
        assert_eq!(view.blend, "90.0");
        assert_eq!(view.top1_cos.as_deref(), Some("0.610"));
        assert!(view.exploration);
        assert!(SignalsView::from_json("{}").empty);
        assert!(SignalsView::from_json("not json").empty);
    }

    #[tokio::test]
    async fn overview_shows_last_run_budget_unrated_picks_and_ratings() {
        let seed = seed().await;
        let app = app_with_users(&seed.db).await;
        let body = assert_admin_only(&app, "/dashboard").await;
        assert!(
            body.contains("curation: 8 considered → 6 eligible"),
            "{body}"
        );
        assert!(body.contains("admission: triage 3"), "{body}");
        assert!(
            body.contains(&format!("/dashboard/runs/{}", seed.run_id)),
            "{body}"
        );
        assert!(body.contains("1 warning"), "{body}");
        // Article 2 is published but unrated; article 1 carries a rating.
        assert!(body.contains("Article 2 about graphs"), "{body}");
        assert!(!body.contains("Article 1 about prose"), "{body}");
        assert!(body.contains("name=\"article_id\" value=\"2\""), "{body}");
        assert!(body.contains("deepseek"), "{body}");
        assert!(body.contains("voyage"), "{body}");
        assert!(body.contains("Ratings this week"), "{body}");
        // Step 6: three sparklines over the last 30 runs (two finished here).
        assert!(body.contains("Cost per run"), "{body}");
        assert!(body.contains("Generation time"), "{body}");
        assert_eq!(body.matches("<polyline").count(), 3, "{body}");
        assert!(body.contains("2 runs · max $0.11"), "{body}");
        assert!(!body.contains("style=\""), "no inline styles under the CSP");
    }

    #[tokio::test]
    async fn every_dashboard_route_template_renders_with_fixture_data() {
        let seed = seed().await;
        sqlx::query(
            "INSERT INTO jobs
             (id, name, unit, requested_at, started_at, finished_at, status, message, run_id)
             VALUES (99, 'features-prune', 'daily-epub-job@features-prune.service',
                     '2026-09-02T06:00:00Z', '2026-09-02T06:00:01Z',
                     '2026-09-02T06:00:02Z', 'ok', 'pruned fixture rows', ?)",
        )
        .bind(seed.run_id)
        .execute(seed.db.pool())
        .await
        .unwrap();
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let routes = vec![
            "/dashboard".to_string(),
            "/dashboard/runs".to_string(),
            format!("/dashboard/runs/{}", seed.run_id),
            "/dashboard/articles".to_string(),
            "/dashboard/articles/1".to_string(),
            "/dashboard/ratings".to_string(),
            "/dashboard/ratings?tab=events".to_string(),
            "/dashboard/profile".to_string(),
            "/dashboard/stats?days=14".to_string(),
            "/dashboard/settings".to_string(),
            "/dashboard/settings/history".to_string(),
            "/dashboard/jobs".to_string(),
            "/dashboard/jobs/99".to_string(),
            "/dashboard/users".to_string(),
        ];
        for uri in routes {
            let response = get(&app, &uri, Some(&admin)).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let body = response_text(response).await;
            assert!(body.contains("<!doctype html>"), "{uri}: {body}");
            assert!(body.contains("The Daily EPUB"), "{uri}: {body}");
        }
    }
}
