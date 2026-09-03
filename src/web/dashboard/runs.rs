//! Dashboard: runs list and run detail (dashboard plan §9.2).
//!
//! The detail page renders the same `candidate_runs` telemetry that
//! `explain` prints: the funnel, the report's admission/preference/timing/
//! provider blocks, the config diff against the previous non-dry run, the
//! near misses via `telemetry::near_misses`, and the candidates table with
//! allow-listed filters and sorts.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use serde::Deserialize;
use sqlx::Row as _;

use super::{
    Bind, Pager, REASONS, RETRIEVERS, STAGES, SignalsView, admitted_by_parts, allow_listed,
    bind_all, db_err, duration_between, dynamic_query, fmt_duration, fmt_opt, fmt_opt_int,
    fmt_stored_time, fmt_usd, like_pattern, non_empty, page_number,
};
use crate::config::Config;
use crate::curate::telemetry;
use crate::db::Db;
use crate::report::RunReport;
use crate::server::AppState;
use crate::types::ArticleId;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, Pagination, WebError, take_flash};

const RUNS_PER_PAGE: u32 = 50;
const CANDIDATES_PER_PAGE: u32 = 100;
const NEAR_MISSES: usize = 10;
const TOP_FEEDS: usize = 20;

/// `runs.status` values accepted by `?status=`.
const STATUSES: [&str; 5] = ["running", "ok", "degraded", "failed", "dry_run"];

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/runs", get(list))
        .route("/dashboard/runs/{id}", get(detail))
}

// ---------------------------------------------------------------------------
// Runs list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct RunsQuery {
    pub status: Option<String>,
    pub page: Option<u32>,
}

#[derive(Debug, Clone)]
struct RunListRow {
    id: i64,
    date: String,
    status: String,
    started: String,
    duration: String,
    funnel: String,
    costs: String,
    total: String,
    dry_run: bool,
    warnings: usize,
}

#[derive(Template)]
#[template(path = "dashboard/runs.html")]
struct RunsTemplate {
    page: Page,
    runs: Vec<RunListRow>,
    status: String,
    statuses: Vec<&'static str>,
    pager: Pager,
}

/// One `runs` row with its parsed report, when present.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub date: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub entries_fetched: i64,
    pub candidates: i64,
    pub selected: i64,
    pub cost_usd: f64,
    pub error: Option<String>,
    pub config_json: Option<String>,
    pub report: Option<RunReport>,
}

impl RunRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        Self {
            id: row.get("id"),
            date: row.get("date"),
            status: row.get("status"),
            started_at: row.get("started_at"),
            finished_at: row.get("finished_at"),
            entries_fetched: row.get("entries_fetched"),
            candidates: row.get("candidates"),
            selected: row.get("selected"),
            cost_usd: row.get("cost_usd"),
            error: row.get("error"),
            config_json: row.get("config_json"),
            report: row
                .get::<Option<String>, _>("report_json")
                .and_then(|raw| serde_json::from_str::<RunReport>(&raw).ok()),
        }
    }

    pub fn duration_secs(&self) -> Option<i64> {
        duration_between(&self.started_at, self.finished_at.as_deref())
    }

    /// `considered → eligible → triaged → assessed → shortlisted → selected`
    /// from the report, or the legacy counters when it is missing.
    pub fn funnel_line(&self) -> String {
        match &self.report {
            Some(report) => {
                let c = &report.counts;
                format!(
                    "{} → {} → {} → {} → {} → {}",
                    c.articles, c.eligible, c.triaged, c.assessed, c.shortlisted, c.selected
                )
            }
            None => format!(
                "{} entries → {} → {}",
                self.entries_fetched, self.candidates, self.selected
            ),
        }
    }

    pub fn costs_line(&self) -> String {
        self.report
            .as_ref()
            .map(|report| {
                report
                    .provider_costs
                    .iter()
                    .map(|(provider, usage)| format!("{provider} {}", fmt_usd(usage.cost_usd)))
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| "—".into())
    }
}

const RUN_COLUMNS: &str = "id, date, status, started_at, finished_at, entries_fetched, candidates,
       selected, cost_usd, error, config_json, report_json";

pub async fn list_runs(
    db: &Db,
    status: Option<&str>,
    page: u32,
) -> Result<(Vec<RunRow>, Pagination), sqlx::Error> {
    let status = allow_listed(status, &STATUSES);
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE (? IS NULL OR status = ?)")
            .bind(status)
            .bind(status)
            .fetch_one(db.pool())
            .await?;
    let pagination = Pagination {
        page,
        per_page: RUNS_PER_PAGE,
        total,
    };
    let rows = dynamic_query(format!(
        "SELECT {RUN_COLUMNS} FROM runs WHERE (? IS NULL OR status = ?)
         ORDER BY id DESC LIMIT ? OFFSET ?"
    ))
    .bind(status)
    .bind(status)
    .bind(i64::from(RUNS_PER_PAGE))
    .bind(pagination.offset())
    .fetch_all(db.pool())
    .await?;
    Ok((rows.iter().map(RunRow::from_row).collect(), pagination))
}

async fn list(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<RunsQuery>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let status = allow_listed(query.status.as_deref(), &STATUSES).unwrap_or("");
    let page = page_number(query.page);
    let (rows, pagination) = list_runs(&state.db, non_empty(Some(status)), page)
        .await
        .map_err(db_err)?;
    let runs = rows
        .iter()
        .map(|run| RunListRow {
            id: run.id,
            date: run.date.clone(),
            status: run.status.clone(),
            started: fmt_stored_time(Some(&run.started_at), &config),
            duration: fmt_duration(run.duration_secs()),
            funnel: run.funnel_line(),
            costs: run.costs_line(),
            total: fmt_usd(run.cost_usd),
            dry_run: run.status == "dry_run",
            warnings: run
                .report
                .as_ref()
                .map(|report| report.warnings.len())
                .unwrap_or(0),
        })
        .collect();
    let pager = Pager::new(
        pagination,
        "/dashboard/runs",
        &[("status", Some(status.to_string()))],
    );
    let mut page = Page::new("Runs", viewer, "runs");
    page.flash = take_flash(&session).await?;
    Ok(Html(RunsTemplate {
        page,
        runs,
        status: status.to_string(),
        statuses: STATUSES.to_vec(),
        pager,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Funnel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ReasonCount {
    pub reason: String,
    pub count: i64,
}

/// One bar of the funnel: how many rows reached this stage or a later one,
/// how many stopped exactly here, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct FunnelStage {
    pub stage: &'static str,
    pub reached: i64,
    pub stopped: i64,
    /// Bar width as a percentage of the rows considered.
    pub percent: u32,
    pub reasons: Vec<ReasonCount>,
}

/// `SELECT stage, excluded_reason, COUNT(*) … GROUP BY 1, 2` shaped into
/// pipeline order. `reached` is cumulative from the end of the pipeline, so
/// the first bar is every row the run considered.
pub async fn funnel(db: &Db, run_id: i64) -> Result<Vec<FunnelStage>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT stage, excluded_reason, COUNT(*) AS n FROM candidate_runs
         WHERE run_id = ? GROUP BY stage, excluded_reason ORDER BY n DESC, excluded_reason",
    )
    .bind(run_id)
    .fetch_all(db.pool())
    .await?;
    let mut stopped: BTreeMap<&'static str, (i64, Vec<ReasonCount>)> = BTreeMap::new();
    let mut total = 0i64;
    for row in &rows {
        let stage: String = row.get("stage");
        let count: i64 = row.get("n");
        total += count;
        let Some(name) = STAGES.iter().copied().find(|name| *name == stage) else {
            continue;
        };
        let entry = stopped.entry(name).or_insert_with(|| (0, Vec::new()));
        entry.0 += count;
        if let Some(reason) = row.get::<Option<String>, _>("excluded_reason") {
            entry.1.push(ReasonCount { reason, count });
        }
    }
    let mut remaining = total;
    let mut stages = Vec::with_capacity(STAGES.len());
    for (index, stage) in STAGES.iter().copied().enumerate() {
        let (count, reasons) = stopped.remove(stage).unwrap_or_default();
        let reached = if index == 0 { total } else { remaining };
        let percent = if total > 0 {
            ((reached as f64 / total as f64) * 100.0).round() as u32
        } else {
            0
        };
        stages.push(FunnelStage {
            stage,
            reached,
            stopped: count,
            percent,
            reasons,
        });
        remaining -= count;
    }
    Ok(stages)
}

// ---------------------------------------------------------------------------
// Config diff
// ---------------------------------------------------------------------------

/// Flatten a JSON document to dotted keys. Arrays stay whole (as compact
/// JSON) so a reordered list reads as one change.
pub fn flatten_json(value: &serde_json::Value) -> BTreeMap<String, String> {
    fn walk(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, child, out);
                }
            }
            serde_json::Value::Null if prefix.is_empty() => {}
            serde_json::Value::String(text) => {
                out.insert(prefix.to_string(), text.clone());
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk("", value, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub key: String,
    pub before: String,
    pub after: String,
}

/// Keys whose flattened values differ between two config documents; a key
/// missing on one side shows as `—`.
pub fn config_diff(before: &serde_json::Value, after: &serde_json::Value) -> Vec<DiffRow> {
    let before = flatten_json(before);
    let after = flatten_json(after);
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|key| {
            let old = before.get(key);
            let new = after.get(key);
            (old != new).then(|| DiffRow {
                key: key.clone(),
                before: old.cloned().unwrap_or_else(|| "—".into()),
                after: new.cloned().unwrap_or_else(|| "—".into()),
            })
        })
        .collect()
}

fn parse_config(raw: Option<&str>) -> serde_json::Value {
    raw.and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// Candidates table
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct CandidatesQuery {
    pub stage: Option<String>,
    pub reason: Option<String>,
    pub admitted_by: Option<String>,
    pub q: Option<String>,
    pub flag: Option<String>,
    pub sort: Option<String>,
    pub page: Option<u32>,
}

/// Validated candidate filters: every value is allow-listed or free text
/// bound as a LIKE pattern; `sort` is the allow-list name, never SQL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateFilters {
    pub stage: Option<String>,
    pub reason: Option<String>,
    pub admitted_by: Option<String>,
    pub q: Option<String>,
    pub flag: Option<String>,
    pub sort: &'static str,
}

const CANDIDATE_SORTS: [(&str, &str); 6] = [
    ("utility", "cr.utility DESC, cr.article_id ASC"),
    (
        "rank",
        "cr.rank_utility IS NULL, cr.rank_utility ASC, cr.article_id ASC",
    ),
    ("triage", "t.score DESC, cr.article_id ASC"),
    ("quality", "d.score DESC, cr.article_id ASC"),
    ("fit", "d.fit DESC, cr.article_id ASC"),
    ("title", "a.title COLLATE NOCASE ASC, cr.article_id ASC"),
];

const FLAGS: [&str; 2] = ["exploration", "auto"];

impl CandidateFilters {
    pub fn from_query(query: &CandidatesQuery) -> Self {
        let owned = |value: Option<&str>| value.map(str::to_string);
        Self {
            stage: owned(allow_listed(query.stage.as_deref(), &STAGES)),
            reason: owned(allow_listed(query.reason.as_deref(), &REASONS)),
            admitted_by: owned(allow_listed(query.admitted_by.as_deref(), &RETRIEVERS)),
            q: owned(non_empty(query.q.as_deref())),
            flag: owned(allow_listed(query.flag.as_deref(), &FLAGS)),
            sort: CANDIDATE_SORTS
                .iter()
                .find(|(name, _)| Some(*name) == query.sort.as_deref())
                .map(|(name, _)| *name)
                .unwrap_or(CANDIDATE_SORTS[0].0),
        }
    }

    fn order_by(&self) -> &'static str {
        CANDIDATE_SORTS
            .iter()
            .find(|(name, _)| *name == self.sort)
            .map(|(_, sql)| *sql)
            .unwrap_or(CANDIDATE_SORTS[0].1)
    }

    /// The `AND …` clauses and their bound values.
    fn where_clauses(&self) -> (String, Vec<Bind>) {
        let mut sql = String::new();
        let mut binds = Vec::new();
        if let Some(stage) = &self.stage {
            sql.push_str(" AND cr.stage = ?");
            binds.push(Bind::Text(stage.clone()));
        }
        if let Some(reason) = &self.reason {
            sql.push_str(" AND cr.excluded_reason = ?");
            binds.push(Bind::Text(reason.clone()));
        }
        if let Some(retriever) = &self.admitted_by {
            sql.push_str(" AND json_extract(cr.admitted_by, '$[0]') LIKE ? ESCAPE '\\'");
            binds.push(Bind::Text(format!("{retriever}%")));
        }
        if let Some(q) = &self.q {
            sql.push_str(" AND a.title LIKE ? ESCAPE '\\'");
            binds.push(Bind::Text(like_pattern(q)));
        }
        match self.flag.as_deref() {
            Some("exploration") => {
                sql.push_str(" AND json_extract(cr.signals_json, '$.exploration') = 1");
            }
            Some("auto") => {
                sql.push_str(" AND json_extract(cr.signals_json, '$.auto_include') = 1");
            }
            _ => {}
        }
        (sql, binds)
    }

    fn params(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("stage", self.stage.clone()),
            ("reason", self.reason.clone()),
            ("admitted_by", self.admitted_by.clone()),
            ("q", self.q.clone()),
            ("flag", self.flag.clone()),
            (
                "sort",
                (self.sort != CANDIDATE_SORTS[0].0).then(|| self.sort.to_string()),
            ),
        ]
    }
}

/// One row of the candidates table.
#[derive(Debug, Clone)]
pub struct CandidateView {
    pub article_id: ArticleId,
    pub title: String,
    pub href: String,
    pub feed: String,
    pub words: i64,
    pub stage: String,
    pub reason: Option<String>,
    pub admitted_first: Option<String>,
    pub admitted_rest: String,
    pub utility: String,
    pub rank: String,
    pub cluster: String,
    pub triage: String,
    pub quality: String,
    pub fit: String,
    pub exploration: bool,
    pub auto_include: bool,
    pub editor_why: Option<String>,
    pub signals: SignalsView,
}

impl CandidateView {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        let article_id: ArticleId = row.get("article_id");
        let (admitted_first, rest) =
            admitted_by_parts(row.get::<Option<String>, _>("admitted_by").as_deref());
        let signals = SignalsView::from_json(&row.get::<String, _>("signals_json"));
        Self {
            article_id,
            title: row.get("title"),
            href: format!("/dashboard/articles/{article_id}"),
            feed: row.get("feed_title"),
            words: row.get("word_count"),
            stage: row.get("stage"),
            reason: row.get("excluded_reason"),
            admitted_first,
            admitted_rest: rest.join(", "),
            utility: fmt_opt(row.get("utility"), 1),
            rank: fmt_opt_int(row.get("rank_utility")),
            cluster: match (
                row.get::<Option<i64>, _>("cluster_id"),
                row.get::<Option<i64>, _>("cluster_rank"),
            ) {
                (Some(id), Some(rank)) => format!("{id} · {rank}"),
                (Some(id), None) => id.to_string(),
                _ => "—".into(),
            },
            triage: fmt_opt(row.get("triage"), 1),
            quality: fmt_opt(row.get("quality"), 1),
            fit: fmt_opt(row.get("fit"), 1),
            exploration: signals.exploration,
            auto_include: signals.auto_include,
            editor_why: row.get("editor_why"),
            signals,
        }
    }
}

const CANDIDATE_FROM: &str = "FROM candidate_runs cr
         JOIN articles a ON a.id = cr.article_id
         LEFT JOIN entries e ON e.id = a.best_entry_id
         LEFT JOIN article_assessments t ON t.article_id = cr.article_id AND t.stage = 'triage'
         LEFT JOIN article_assessments d ON d.article_id = cr.article_id AND d.stage = 'deep'
         WHERE cr.run_id = ?";

pub async fn candidates(
    db: &Db,
    run_id: i64,
    filters: &CandidateFilters,
    page: u32,
) -> Result<(Vec<CandidateView>, Pagination), sqlx::Error> {
    let (clauses, binds) = filters.where_clauses();
    let count_sql = format!("SELECT COUNT(*) {CANDIDATE_FROM}{clauses}");
    let total: i64 = bind_all(dynamic_query(count_sql).bind(run_id), &binds)
        .fetch_one(db.pool())
        .await?
        .get(0);
    let pagination = Pagination {
        page,
        per_page: CANDIDATES_PER_PAGE,
        total,
    };
    let select_sql = format!(
        "SELECT cr.article_id, COALESCE(a.title, '') AS title,
                COALESCE(e.feed_title, '') AS feed_title, a.word_count,
                cr.stage, cr.excluded_reason, cr.admitted_by, cr.signals_json, cr.utility,
                cr.rank_utility, cr.cluster_id, cr.cluster_rank, cr.editor_why,
                t.score AS triage, d.score AS quality, d.fit AS fit
         {CANDIDATE_FROM}{clauses}
         ORDER BY {}
         LIMIT ? OFFSET ?",
        filters.order_by()
    );
    let rows = bind_all(dynamic_query(select_sql).bind(run_id), &binds)
        .bind(i64::from(CANDIDATES_PER_PAGE))
        .bind(pagination.offset())
        .fetch_all(db.pool())
        .await?;
    Ok((
        rows.iter().map(CandidateView::from_row).collect(),
        pagination,
    ))
}

// ---------------------------------------------------------------------------
// Run detail
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RunHeader {
    id: i64,
    date: String,
    status: String,
    started: String,
    finished: String,
    duration: String,
    dry_run: bool,
    issue_href: Option<String>,
    prev_id: Option<i64>,
    next_id: Option<i64>,
    error: Option<String>,
    total_cost: String,
}

#[derive(Debug, Clone)]
struct CountLine {
    name: String,
    count: i64,
}

#[derive(Debug, Clone)]
struct TimingLine {
    stage: String,
    seconds: String,
}

#[derive(Debug, Clone)]
struct ProviderLine {
    provider: String,
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    cost: String,
}

#[derive(Debug, Clone, Default)]
struct PreferenceView {
    rated_with_embeddings: i64,
    knn_gate: String,
    feed_gate: String,
    verdicts_in_prompt: i64,
}

#[derive(Debug, Clone)]
struct NearMissLine {
    href: String,
    title: String,
    feed: String,
    score: String,
    stage: String,
    reason: Option<String>,
    quality: String,
    fit: String,
}

#[derive(Template)]
#[template(path = "dashboard/run.html")]
struct RunTemplate {
    page: Page,
    run: RunHeader,
    has_report: bool,
    funnel: Vec<FunnelStage>,
    admission: Vec<CountLine>,
    preference: PreferenceView,
    timings: Vec<TimingLine>,
    timings_total: String,
    providers: Vec<ProviderLine>,
    warnings: Vec<String>,
    feeds: Vec<CountLine>,
    diff_against: Option<i64>,
    config_diff: Vec<DiffRow>,
    near_misses: Vec<NearMissLine>,
    candidates: Vec<CandidateView>,
    filters: CandidateFilters,
    stages: Vec<&'static str>,
    reasons: Vec<&'static str>,
    retrievers: Vec<&'static str>,
    sorts: Vec<&'static str>,
    pager: Pager,
}

pub async fn run_by_id(db: &Db, id: i64) -> Result<Option<RunRow>, sqlx::Error> {
    let row = dynamic_query(format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?"))
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(RunRow::from_row))
}

/// The nearest earlier run that was not a dry run and recorded its config.
async fn previous_config(db: &Db, id: i64) -> Result<Option<(i64, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, config_json FROM runs
         WHERE id < ? AND status != 'dry_run' AND config_json IS NOT NULL
         ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| (row.get("id"), row.get("config_json"))))
}

async fn neighbour_run(db: &Db, id: i64, next: bool) -> Result<Option<i64>, sqlx::Error> {
    let sql = if next {
        "SELECT id FROM runs WHERE id > ? ORDER BY id ASC LIMIT 1"
    } else {
        "SELECT id FROM runs WHERE id < ? ORDER BY id DESC LIMIT 1"
    };
    let row = sqlx::query(sql).bind(id).fetch_optional(db.pool()).await?;
    Ok(row.map(|row| row.get("id")))
}

async fn detail(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
    Query(query): Query<CandidatesQuery>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config: std::sync::Arc<Config> = state.config();
    let db = &state.db;
    let Some(run) = run_by_id(db, id).await.map_err(db_err)? else {
        return Err(WebError::NotFound);
    };
    let issue_exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM issues WHERE date = ?")
        .bind(&run.date)
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)?;
    let header = RunHeader {
        id: run.id,
        date: run.date.clone(),
        status: run.status.clone(),
        started: fmt_stored_time(Some(&run.started_at), &config),
        finished: fmt_stored_time(run.finished_at.as_deref(), &config),
        duration: fmt_duration(run.duration_secs()),
        dry_run: run.status == "dry_run",
        issue_href: issue_exists.map(|_| format!("/issues/{}", run.date)),
        prev_id: neighbour_run(db, id, false).await.map_err(db_err)?,
        next_id: neighbour_run(db, id, true).await.map_err(db_err)?,
        error: run.error.clone(),
        total_cost: fmt_usd(run.cost_usd),
    };

    let funnel = funnel(db, id).await.map_err(db_err)?;
    let (admission, preference, timings, timings_total, providers, warnings, feeds) =
        match &run.report {
            Some(report) => (
                report
                    .counts
                    .admitted_by
                    .iter()
                    .map(|(name, count)| CountLine {
                        name: name.clone(),
                        count: *count,
                    })
                    .collect(),
                PreferenceView {
                    rated_with_embeddings: report.counts.rated_with_embeddings,
                    knn_gate: format!("{:.2}", report.counts.knn_gate),
                    feed_gate: format!("{:.2}", report.counts.feed_gate),
                    verdicts_in_prompt: report.counts.verdicts_in_prompt,
                },
                report
                    .timings
                    .0
                    .iter()
                    .map(|(stage, millis)| TimingLine {
                        stage: stage.clone(),
                        seconds: format!("{:.1}", *millis as f64 / 1000.0),
                    })
                    .collect(),
                format!("{:.1}", report.timings.total_ms() as f64 / 1000.0),
                report
                    .provider_costs
                    .iter()
                    .map(|(provider, usage)| ProviderLine {
                        provider: provider.clone(),
                        input: usage.usage.input_tokens,
                        cached: usage.usage.cached_tokens,
                        cache_write: usage.usage.cache_write_tokens,
                        output: usage.usage.output_tokens,
                        cost: fmt_usd(usage.cost_usd),
                    })
                    .collect(),
                report.warnings.clone(),
                report
                    .top_feeds(TOP_FEEDS)
                    .into_iter()
                    .map(|(name, count)| CountLine {
                        name: name.to_string(),
                        count,
                    })
                    .collect(),
            ),
            None => (
                Vec::new(),
                PreferenceView::default(),
                Vec::new(),
                "—".into(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        };

    let previous = previous_config(db, id).await.map_err(db_err)?;
    let (diff_against, config_diff) = match &previous {
        Some((previous_id, previous_json)) => (
            Some(*previous_id),
            config_diff(
                &parse_config(Some(previous_json)),
                &parse_config(run.config_json.as_deref()),
            ),
        ),
        None => (None, Vec::new()),
    };

    let near_misses = telemetry::near_misses(db, id, NEAR_MISSES)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|row| {
            let signals = row.signals();
            let raw = |name: &str| signals.as_ref().and_then(|s| s.raw.get(name).copied());
            NearMissLine {
                href: format!("/dashboard/articles/{}", row.article_id),
                title: row.title.clone(),
                feed: row.feed_title.clone(),
                score: fmt_opt(row.score(), 1),
                stage: row.stage.clone(),
                reason: row.excluded_reason.clone(),
                quality: fmt_opt(raw("quality"), 1),
                fit: fmt_opt(raw("fit"), 1),
            }
        })
        .collect();

    let filters = CandidateFilters::from_query(&query);
    let page_no = page_number(query.page);
    let (candidates, pagination) = candidates(db, id, &filters, page_no)
        .await
        .map_err(db_err)?;
    let pager = Pager::new(
        pagination,
        &format!("/dashboard/runs/{id}"),
        &filters.params(),
    );

    let mut page = Page::new(format!("Run {id} · {}", run.date), viewer, "runs");
    page.flash = take_flash(&session).await?;
    Ok(Html(RunTemplate {
        page,
        run: header,
        has_report: run.report.is_some(),
        funnel,
        admission,
        preference,
        timings,
        timings_total,
        providers,
        warnings,
        feeds,
        diff_against,
        config_diff,
        near_misses,
        candidates,
        filters,
        stages: STAGES.to_vec(),
        reasons: REASONS.to_vec(),
        retrievers: RETRIEVERS.to_vec(),
        sorts: CANDIDATE_SORTS.iter().map(|(name, _)| *name).collect(),
        pager,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, get, login_cookie, seed,
    };

    #[tokio::test]
    async fn funnel_counts_match_the_seeded_rows() {
        let seed = seed().await;
        let stages = funnel(&seed.db, seed.run_id).await.unwrap();
        let by_name = |name: &str| stages.iter().find(|stage| stage.stage == name).unwrap();
        assert_eq!(by_name("excluded").reached, 8);
        assert_eq!(by_name("excluded").stopped, 2);
        assert_eq!(by_name("excluded").percent, 100);
        assert_eq!(
            by_name("excluded").reasons,
            vec![
                ReasonCount {
                    reason: "blocked".into(),
                    count: 1
                },
                ReasonCount {
                    reason: "published_before".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(by_name("eligible").reached, 6);
        assert_eq!(by_name("eligible").stopped, 0);
        assert_eq!(by_name("triaged").reached, 6);
        assert_eq!(by_name("triaged").stopped, 2);
        assert_eq!(by_name("triaged").reasons[0].reason, "not_admitted");
        assert_eq!(by_name("admitted").reached, 4);
        assert_eq!(by_name("assessed").reached, 4);
        assert_eq!(by_name("assessed").stopped, 1);
        assert_eq!(by_name("shortlisted").reached, 3);
        assert_eq!(by_name("selected").reached, 2);
        assert_eq!(by_name("selected").stopped, 2);
        assert_eq!(by_name("selected").percent, 25);
        assert!(by_name("selected").reasons.is_empty());

        let empty = funnel(&seed.db, 999).await.unwrap();
        assert_eq!(empty.len(), STAGES.len());
        assert!(
            empty
                .iter()
                .all(|stage| stage.reached == 0 && stage.percent == 0)
        );
    }

    #[tokio::test]
    async fn candidate_filters_and_sorts_are_allow_listed() {
        let seed = seed().await;
        let db = &seed.db;
        let query = |stage: Option<&str>, sort: Option<&str>| CandidatesQuery {
            stage: stage.map(str::to_string),
            sort: sort.map(str::to_string),
            ..CandidatesQuery::default()
        };

        let filters = CandidateFilters::from_query(&query(None, None));
        assert_eq!(filters.sort, "utility");
        let (all, pagination) = candidates(db, seed.run_id, &filters, 1).await.unwrap();
        assert_eq!(pagination.total, 8);
        assert_eq!(all[0].article_id, 1, "utility desc, NULLs last");
        assert_eq!(all[3].article_id, 4);
        assert!(all[4..].iter().all(|row| row.utility == "—"));

        let unknown = CandidateFilters::from_query(&query(
            Some("selected; DROP TABLE runs"),
            Some("article_id; DROP TABLE runs"),
        ));
        assert_eq!(unknown.stage, None);
        assert_eq!(unknown.sort, "utility");
        let (rows, _) = candidates(db, seed.run_id, &unknown, 1).await.unwrap();
        assert_eq!(rows.len(), 8, "an unknown sort falls back, never errors");

        let selected = CandidateFilters::from_query(&query(Some("selected"), Some("title")));
        let (rows, pagination) = candidates(db, seed.run_id, &selected, 1).await.unwrap();
        assert_eq!(pagination.total, 2);
        assert_eq!(rows[0].article_id, 1);
        assert_eq!(rows[0].admitted_first.as_deref(), Some("triage"));
        assert_eq!(rows[0].admitted_rest, "interest");
        assert_eq!(rows[0].editor_why.as_deref(), Some("Why one"));

        let by_rank = CandidateFilters::from_query(&query(None, Some("rank")));
        let (rows, _) = candidates(db, seed.run_id, &by_rank, 1).await.unwrap();
        assert_eq!(rows[0].rank, "1");
        assert_eq!(rows[7].rank, "—");

        let by_quality = CandidateFilters::from_query(&query(None, Some("quality")));
        let (rows, _) = candidates(db, seed.run_id, &by_quality, 1).await.unwrap();
        assert_eq!(rows[0].article_id, 1, "deep quality 8.8 first");
        assert_eq!(rows[0].quality, "8.8");
        assert_eq!(rows[1].article_id, 4);

        let reason = CandidateFilters::from_query(&CandidatesQuery {
            reason: Some("not_admitted".into()),
            ..CandidatesQuery::default()
        });
        let (rows, _) = candidates(db, seed.run_id, &reason, 1).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.stage == "triaged"));

        let retriever = CandidateFilters::from_query(&CandidatesQuery {
            admitted_by: Some("blend".into()),
            ..CandidatesQuery::default()
        });
        let (rows, _) = candidates(db, seed.run_id, &retriever, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].article_id, 2);

        let unknown_retriever = CandidateFilters::from_query(&CandidatesQuery {
            admitted_by: Some("' OR 1=1 --".into()),
            ..CandidatesQuery::default()
        });
        assert_eq!(unknown_retriever.admitted_by, None);

        let title = CandidateFilters::from_query(&CandidatesQuery {
            q: Some("100% graphs".into()),
            ..CandidatesQuery::default()
        });
        let (rows, _) = candidates(db, seed.run_id, &title, 1).await.unwrap();
        assert!(rows.is_empty(), "the % is literal, not a wildcard");
        let title = CandidateFilters::from_query(&CandidatesQuery {
            q: Some("graphs".into()),
            ..CandidatesQuery::default()
        });
        let (rows, _) = candidates(db, seed.run_id, &title, 1).await.unwrap();
        assert_eq!(rows.len(), 4);

        let exploration = CandidateFilters::from_query(&CandidatesQuery {
            flag: Some("exploration".into()),
            ..CandidatesQuery::default()
        });
        let (rows, _) = candidates(db, seed.run_id, &exploration, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].article_id, 2);
        assert!(rows[0].exploration);
        assert!(rows[0].signals.lines.iter().any(|line| line.present));

        let thin = all.iter().find(|row| row.article_id == 7).unwrap();
        assert!(thin.signals.empty);
        assert_eq!(thin.reason.as_deref(), Some("blocked"));
    }

    #[test]
    fn config_diff_finds_changed_dotted_keys_and_ignores_unchanged() {
        let before = json!({
            "curation": {"deep_keep": 100, "shortlist_keep": 60, "sections": ["A", "B"],
                         "weights": {"quality": 0.4, "fit": 0.2}},
            "llm": {"bulk": "deepseek"},
            "flag": null
        });
        let after = json!({
            "curation": {"deep_keep": 120, "shortlist_keep": 60, "sections": ["B", "A"],
                         "weights": {"quality": 0.4, "fit": 0.25}},
            "llm": {"bulk": "deepseek", "editor": "anthropic"}
        });
        let diff = config_diff(&before, &after);
        let keys: Vec<&str> = diff.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "curation.deep_keep",
                "curation.sections",
                "curation.weights.fit",
                "flag",
                "llm.editor"
            ]
        );
        assert_eq!(diff[0].before, "100");
        assert_eq!(diff[0].after, "120");
        assert_eq!(diff[1].before, "[\"A\",\"B\"]");
        assert_eq!(diff[3].before, "null");
        assert_eq!(diff[3].after, "—");
        assert_eq!(diff[4].before, "—");
        assert_eq!(diff[4].after, "anthropic");
        assert!(config_diff(&before, &before).is_empty());
        assert!(config_diff(&json!(null), &json!(null)).is_empty());
        assert_eq!(flatten_json(&json!({"a": {"b": "x"}}))["a.b"], "x");
    }

    #[tokio::test]
    async fn runs_pages_are_admin_only_and_render_the_seeded_run() {
        let seed = seed().await;
        let app = app_with_users(&seed.db).await;
        let list = assert_admin_only(&app, "/dashboard/runs").await;
        assert!(list.contains("8 → 6 → 6 → 4 → 3 → 2"), "{list}");
        assert!(list.contains("deepseek $0.11"), "{list}");
        assert!(
            list.contains(&format!("/dashboard/runs/{}", seed.run_id)),
            "{list}"
        );

        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let filtered = get(&app, "/dashboard/runs?status=failed", Some(&admin)).await;
        let filtered = crate::web::dashboard::tests::response_text(filtered).await;
        assert!(!filtered.contains("8 → 6 → 6"), "{filtered}");
        let unknown = get(&app, "/dashboard/runs?status=nope&page=0", Some(&admin)).await;
        assert_eq!(unknown.status(), axum::http::StatusCode::OK);

        let detail = assert_admin_only(&app, &format!("/dashboard/runs/{}", seed.run_id)).await;
        assert!(
            detail.contains("social: lobsters lookup timed out"),
            "{detail}"
        );
        assert!(detail.contains("curation.deep_keep"), "{detail}");
        assert!(detail.contains("llm.editor"), "{detail}");
        assert!(
            detail.contains(&format!("run {}", seed.earlier_run_id)),
            "{detail}"
        );
        assert!(
            detail.contains("Article 3 about prose"),
            "near miss: {detail}"
        );
        assert!(detail.contains("Gaussian Splatting"), "signals: {detail}");
        assert!(detail.contains("Why one"), "{detail}");
        assert!(detail.contains("Alpha Blog"), "{detail}");
        assert!(detail.contains("class=\"funnel\""), "{detail}");
        assert!(
            detail.contains(&format!("/issues/{}", seed.date)),
            "{detail}"
        );
        assert!(
            detail.contains(&format!("/dashboard/runs/{}", seed.earlier_run_id)),
            "prev link: {detail}"
        );

        let filtered = get(
            &app,
            &format!(
                "/dashboard/runs/{}?stage=selected&sort=bogus&q=%25",
                seed.run_id
            ),
            Some(&admin),
        )
        .await;
        assert_eq!(filtered.status(), axum::http::StatusCode::OK);
        let filtered = crate::web::dashboard::tests::response_text(filtered).await;
        assert!(
            !filtered.contains("Article 3 about prose</a></summary>"),
            "{filtered}"
        );

        let missing = get(&app, "/dashboard/runs/999", Some(&admin)).await;
        assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
