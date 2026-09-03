//! Per-run candidate telemetry: the `candidate_runs` writer, `signals_json`,
//! the `explain` command and feature retention (plan §7.4–7.5, §15.2, §16).
//!
//! One row per considered article per run says where it stopped and why. Rows
//! are upserted on every stage transition with every column set (never
//! `COALESCE`), so the last write for a run is the whole truth.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use sqlx::Row as _;

use crate::curate::signals::{Neighbour, Signals, TopInterest};
use crate::db::{Db, fmt_ts};
use crate::report::RunReport;
use crate::types::{ArticleId, Candidate, NearMiss};

/// Signal names rendered by `explain`, in the order of §7.5.
const RENDERED_SIGNALS: [&str; 8] = [
    "interest",
    "knn",
    "feed",
    "social",
    "heuristic",
    "triage",
    "quality",
    "fit",
];

/// One `candidate_runs` row (§7.4).
#[derive(Debug, Clone)]
pub struct CandidateRun<'a> {
    pub run_id: i64,
    pub article_id: ArticleId,
    pub stage: &'a str,
    pub excluded_reason: Option<&'a str>,
    /// JSON array of retriever names, first = the one that admitted it.
    pub admitted_by: Option<&'a str>,
    pub signals_json: &'a str,
    pub utility: Option<f64>,
    pub rank_utility: Option<i64>,
    pub cluster_id: Option<i64>,
    pub cluster_rank: Option<i64>,
    pub editor_why: Option<&'a str>,
}

/// Upsert one row, setting every column (§7.4).
pub async fn write(db: &Db, row: &CandidateRun<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO candidate_runs
             (run_id, article_id, stage, excluded_reason, admitted_by, signals_json,
              utility, rank_utility, cluster_id, cluster_rank, editor_why)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(run_id, article_id) DO UPDATE SET
             stage = excluded.stage,
             excluded_reason = excluded.excluded_reason,
             admitted_by = excluded.admitted_by,
             signals_json = excluded.signals_json,
             utility = excluded.utility,
             rank_utility = excluded.rank_utility,
             cluster_id = excluded.cluster_id,
             cluster_rank = excluded.cluster_rank,
             editor_why = excluded.editor_why",
    )
    .bind(row.run_id)
    .bind(row.article_id)
    .bind(row.stage)
    .bind(row.excluded_reason)
    .bind(row.admitted_by)
    .bind(row.signals_json)
    .bind(row.utility)
    .bind(row.rank_utility)
    .bind(row.cluster_id)
    .bind(row.cluster_rank)
    .bind(row.editor_why)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// The thin row of a hygiene exclusion: keys, `stage = 'excluded'`, the reason
/// and `signals_json = '{}'` (§8.1).
pub async fn thin_excluded(
    db: &Db,
    run_id: i64,
    article_id: ArticleId,
    reason: &str,
) -> Result<(), sqlx::Error> {
    write(
        db,
        &CandidateRun {
            run_id,
            article_id,
            stage: "excluded",
            excluded_reason: Some(reason),
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
}

/// `signals_json` (§7.5). Missing signals are absent from `raw`/`norm` and
/// `false` in `present`; `weights` are the effective weights.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalsJson {
    pub v: i64,
    #[serde(default)]
    pub raw: BTreeMap<String, f64>,
    #[serde(default)]
    pub norm: BTreeMap<String, f64>,
    #[serde(default)]
    pub present: BTreeMap<String, bool>,
    #[serde(default)]
    pub weights: BTreeMap<String, f64>,
    #[serde(default)]
    pub top_interests: Vec<TopInterest>,
    #[serde(default)]
    pub neighbours: Vec<Neighbour>,
    #[serde(default)]
    pub exploration: bool,
    #[serde(default)]
    pub auto_include: bool,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl SignalsJson {
    /// The blend implied by the stored effective weights, 0–100.
    pub fn blend(&self) -> Option<f64> {
        let mut score = 0.0;
        let mut any = false;
        for (name, weight) in &self.weights {
            if let Some(value) = self.norm.get(name) {
                score += weight * value;
                any = true;
            }
        }
        any.then_some(score * 100.0)
    }
}

/// Serialize the signals of §7.5 for one article.
pub fn serialize_signals(signals: &Signals, auto_include: bool) -> String {
    let mut raw = BTreeMap::new();
    for name in [
        "interest",
        "interest_top1_cos",
        "knn",
        "feed",
        "social",
        "heuristic",
    ] {
        if let Some(value) = signals.raw(name) {
            raw.insert(name.to_string(), value);
        }
    }
    let present = RENDERED_SIGNALS
        .into_iter()
        .map(|name| (name.to_string(), signals.present(name)))
        .collect();
    serde_json::to_string(&SignalsJson {
        v: 1,
        raw,
        norm: signals.norm.clone(),
        present,
        weights: signals.weights.clone(),
        top_interests: signals.top_interests.clone(),
        neighbours: signals.neighbours.clone(),
        exploration: false,
        auto_include,
        notes: signals.notes.clone(),
    })
    .unwrap_or_else(|_| "{}".into())
}

/// Serialize a full candidate, adding the triage assessment and admission flags
/// that are not cheap-signal fields (§7.5).
pub fn serialize_candidate(candidate: &Candidate) -> String {
    let base = serialize_signals(&candidate.signals, candidate.auto_include);
    let mut value: SignalsJson = serde_json::from_str(&base).unwrap_or_default();
    value.exploration = candidate.exploration;
    if let Some(triage) = candidate.assessment.triage.as_ref() {
        value.raw.insert("triage".into(), triage.interest);
        value
            .norm
            .insert("triage".into(), (triage.interest / 10.0).clamp(0.0, 1.0));
        value.present.insert("triage".into(), true);
    }
    if let Some(deep) = candidate.assessment.deep.as_ref() {
        value.raw.insert("quality".into(), deep.quality);
        value.raw.insert("fit".into(), deep.fit);
        value
            .norm
            .insert("quality".into(), (deep.quality / 10.0).clamp(0.0, 1.0));
        value
            .norm
            .insert("fit".into(), (deep.fit / 10.0).clamp(0.0, 1.0));
        value.present.insert("quality".into(), true);
        value.present.insert("fit".into(), true);
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------------------
// `explain` (§15.2)
// ---------------------------------------------------------------------------

/// A `candidate_runs` row joined to its article title.
#[derive(Debug, Clone)]
pub struct ExplainRow {
    pub run_id: i64,
    pub article_id: ArticleId,
    pub title: String,
    /// The best entry's feed, for the paper's near-miss list.
    pub feed_title: String,
    pub stage: String,
    pub excluded_reason: Option<String>,
    pub admitted_by: Option<String>,
    pub signals_json: String,
    pub utility: Option<f64>,
    pub rank_utility: Option<i64>,
    pub cluster_id: Option<i64>,
    pub cluster_rank: Option<i64>,
    pub editor_why: Option<String>,
}

impl ExplainRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        Self {
            run_id: row.get("run_id"),
            article_id: row.get("article_id"),
            title: row.get("title"),
            feed_title: row.get("feed_title"),
            stage: row.get("stage"),
            excluded_reason: row.get("excluded_reason"),
            admitted_by: row.get("admitted_by"),
            signals_json: row.get("signals_json"),
            utility: row.get("utility"),
            rank_utility: row.get("rank_utility"),
            cluster_id: row.get("cluster_id"),
            cluster_rank: row.get("cluster_rank"),
            editor_why: row.get("editor_why"),
        }
    }

    pub fn signals(&self) -> Option<SignalsJson> {
        serde_json::from_str(&self.signals_json).ok()
    }

    /// Utility once the deep set has been ranked, else the preliminary blend.
    pub fn score(&self) -> Option<f64> {
        self.utility
            .or_else(|| self.signals().and_then(|signals| signals.blend()))
    }
}

/// The run `explain` reads: `--run-id` when given (and of that date), else the
/// latest non-dry run of the date.
pub async fn resolve_run(
    db: &Db,
    date: Date,
    requested: Option<i64>,
) -> Result<Option<i64>, sqlx::Error> {
    let row = match requested {
        Some(run_id) => {
            sqlx::query("SELECT id FROM runs WHERE id = ? AND date = ?")
                .bind(run_id)
                .bind(date.to_string())
                .fetch_optional(db.pool())
                .await?
        }
        None => {
            sqlx::query(
                "SELECT id FROM runs WHERE date = ? AND status != 'dry_run'
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(date.to_string())
            .fetch_optional(db.pool())
            .await?
        }
    };
    Ok(row.map(|row| row.get("id")))
}

pub async fn explain_row(
    db: &Db,
    run_id: i64,
    article_id: ArticleId,
) -> Result<Option<ExplainRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT cr.run_id, cr.article_id, COALESCE(a.title, '') AS title,
                COALESCE(e.feed_title, '') AS feed_title,
                cr.stage, cr.excluded_reason, cr.admitted_by, cr.signals_json,
                cr.utility, cr.rank_utility, cr.cluster_id, cr.cluster_rank, cr.editor_why
         FROM candidate_runs cr JOIN articles a ON a.id = cr.article_id
         LEFT JOIN entries e ON e.id = a.best_entry_id
         WHERE cr.run_id = ? AND cr.article_id = ?",
    )
    .bind(run_id)
    .bind(article_id)
    .fetch_optional(db.pool())
    .await?;
    Ok(row.as_ref().map(ExplainRow::from_row))
}

/// The top `limit` rows that were not selected, by utility, falling back to
/// the preliminary blend for rows the ranker never reached (§15.2).
pub async fn near_misses(
    db: &Db,
    run_id: i64,
    limit: usize,
) -> Result<Vec<ExplainRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT cr.run_id, cr.article_id, COALESCE(a.title, '') AS title,
                COALESCE(e.feed_title, '') AS feed_title,
                cr.stage, cr.excluded_reason, cr.admitted_by, cr.signals_json,
                cr.utility, cr.rank_utility, cr.cluster_id, cr.cluster_rank, cr.editor_why
         FROM candidate_runs cr JOIN articles a ON a.id = cr.article_id
         LEFT JOIN entries e ON e.id = a.best_entry_id
         WHERE cr.run_id = ? AND cr.stage != 'selected' AND cr.stage != 'excluded'",
    )
    .bind(run_id)
    .fetch_all(db.pool())
    .await?;
    let mut output = rows.iter().map(ExplainRow::from_row).collect::<Vec<_>>();
    output.sort_by(|left, right| {
        right
            .score()
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&left.score().unwrap_or(f64::NEG_INFINITY))
            .then_with(|| left.article_id.cmp(&right.article_id))
    });
    output.truncate(limit);
    Ok(output)
}

fn fmt_opt(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.3}"))
        .unwrap_or_else(|| "—".into())
}

/// Render one persisted row the way §15.2 lists it.
pub async fn render_explain(db: &Db, row: &ExplainRow) -> Result<String, sqlx::Error> {
    let mut out = String::new();
    let _ = writeln!(out, "article {}: {}", row.article_id, row.title);
    let _ = write!(out, "run {} · stage: {}", row.run_id, row.stage);
    if let Some(reason) = &row.excluded_reason {
        let _ = write!(out, " · reason: {reason}");
    }
    let _ = writeln!(out);
    if let Some(signals) = row.signals() {
        let _ = writeln!(out, "signals (raw · norm · weight):");
        for name in RENDERED_SIGNALS {
            let present = signals.present.get(name).copied().unwrap_or(false);
            if present {
                let _ = writeln!(
                    out,
                    "  {name:<10} {:>8} · {:>6} · {:>6}",
                    fmt_opt(signals.raw.get(name).copied()),
                    fmt_opt(signals.norm.get(name).copied()),
                    fmt_opt(signals.weights.get(name).copied()),
                );
            } else {
                let _ = writeln!(out, "  {name:<10}   absent");
            }
        }
        if let Some(blend) = signals.blend() {
            let _ = writeln!(out, "preliminary blend: {blend:.1}");
        }
        if let Some(cos) = signals.raw.get("interest_top1_cos") {
            let _ = writeln!(out, "interest top-1 cosine: {cos:.3}");
        }
        if !signals.top_interests.is_empty() {
            let _ = writeln!(out, "top interests:");
            for interest in &signals.top_interests {
                let _ = writeln!(
                    out,
                    "  {} · z {:.2} · cos {:.3}",
                    interest.name, interest.z, interest.cos
                );
            }
        }
        if !signals.neighbours.is_empty() {
            let _ = writeln!(out, "nearest rated neighbours:");
            for neighbour in &signals.neighbours {
                let _ = writeln!(
                    out,
                    "  {} · cos {:.3} · article {} · {}",
                    neighbour.label, neighbour.cos, neighbour.article_id, neighbour.title
                );
            }
        }
        if signals.exploration || signals.auto_include {
            let _ = writeln!(
                out,
                "flags: exploration={} auto_include={}",
                signals.exploration, signals.auto_include
            );
        }
        for note in &signals.notes {
            let _ = writeln!(out, "note: {note}");
        }
    }

    let assessments = sqlx::query(
        "SELECT stage, model, score, fit, kind, facets_json, rationale, category,
                paywalled_guess, assessed_at
         FROM article_assessments WHERE article_id = ? ORDER BY stage",
    )
    .bind(row.article_id)
    .fetch_all(db.pool())
    .await?;
    if !assessments.is_empty() {
        let _ = writeln!(out, "assessments:");
        for assessment in assessments {
            let stage = assessment.get::<String, _>("stage");
            let model = assessment.get::<String, _>("model");
            let score = fmt_opt(assessment.get::<Option<f64>, _>("score"));
            let rationale = assessment
                .get::<Option<String>, _>("rationale")
                .unwrap_or_default();
            if assessment.get::<Option<String>, _>("kind").as_deref()
                == Some(super::triage::PROVIDER_REJECTED)
            {
                let _ = writeln!(out, "  {stage}: rejected by provider — {rationale}");
                continue;
            }
            if stage == "deep" {
                let _ = writeln!(
                    out,
                    "  deep · {model} · quality {score} · fit {} · format {} · category {} · paywalled={} · {rationale}",
                    fmt_opt(assessment.get::<Option<f64>, _>("fit")),
                    assessment
                        .get::<Option<String>, _>("kind")
                        .unwrap_or_else(|| "—".into()),
                    assessment
                        .get::<Option<String>, _>("category")
                        .unwrap_or_else(|| "—".into()),
                    assessment.get::<i64, _>("paywalled_guess") != 0,
                );
            } else {
                let _ = writeln!(
                    out,
                    "  triage · {model} · interest {score} · kind {} · {rationale}",
                    assessment
                        .get::<Option<String>, _>("kind")
                        .unwrap_or_else(|| "—".into()),
                );
            }
            if let Some(facets) = assessment.get::<Option<String>, _>("facets_json") {
                let _ = writeln!(out, "    facets: {facets}");
            }
        }
    }
    if row.utility.is_some() || row.rank_utility.is_some() {
        let _ = writeln!(
            out,
            "utility: {} · rank {}",
            fmt_opt(row.utility),
            row.rank_utility
                .map(|r| r.to_string())
                .unwrap_or_else(|| "—".into())
        );
    }
    if let Some(cluster) = row.cluster_id {
        let _ = writeln!(
            out,
            "cluster: {cluster} · rank {}",
            row.cluster_rank
                .map(|r| r.to_string())
                .unwrap_or_else(|| "—".into())
        );
    }
    if let Some(admitted_by) = &row.admitted_by {
        let _ = writeln!(out, "admitted by: {admitted_by}");
    }
    if let Some(why) = &row.editor_why {
        let _ = writeln!(out, "editor: {why}");
    }
    Ok(out)
}

/// What `explain` was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainTarget {
    Article(ArticleId),
    Url(String),
}

/// `explain --date D (--article ID | --url URL) [--run-id N]` as text (§15.2).
pub async fn explain(
    db: &Db,
    date: Date,
    run_id: Option<i64>,
    target: &ExplainTarget,
) -> anyhow::Result<String> {
    let article_id = match target {
        ExplainTarget::Article(id) => *id,
        ExplainTarget::Url(url) => {
            let canonical = crate::dedupe::canonical_url(url)
                .ok_or_else(|| anyhow::anyhow!("invalid article URL {url:?}"))?;
            match db.article_id_for_url(&canonical).await? {
                Some(id) => id,
                None => {
                    return Ok(format!(
                        "{canonical} was never ingested: it is not in `articles`, so this is a feed problem, not a ranking problem."
                    ));
                }
            }
        }
    };
    if db.get_article(article_id).await?.is_none() {
        return Ok(format!(
            "article {article_id} was never ingested: it is not in `articles`."
        ));
    }
    let Some(run_id) = resolve_run(db, date, run_id).await? else {
        return Ok(match run_id {
            Some(id) => format!("run {id} is not a run for {date}"),
            None => format!("no non-dry run recorded for {date}"),
        });
    };
    match explain_row(db, run_id, article_id).await? {
        Some(row) => Ok(render_explain(db, &row).await?),
        None => Ok(format!(
            "article {article_id} was not considered by run {run_id} for {date} (outside its ingest window, or telemetry pruned)."
        )),
    }
}

/// `explain --date D --near-misses [N]` as text (§15.2).
pub async fn explain_near_misses(
    db: &Db,
    date: Date,
    run_id: Option<i64>,
    limit: usize,
) -> anyhow::Result<String> {
    let Some(run_id) = resolve_run(db, date, run_id).await? else {
        return Ok(format!("no non-dry run recorded for {date}"));
    };
    let rows = near_misses(db, run_id, limit).await?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "run {run_id} · {date} · top {} not selected, by {}:",
        rows.len(),
        if rows.iter().any(|row| row.utility.is_some()) {
            "utility"
        } else {
            "preliminary blend"
        }
    );
    for (index, row) in rows.iter().enumerate() {
        let reason = row
            .excluded_reason
            .as_deref()
            .map(|reason| format!(", {reason}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{:>3}. {:>6} · {} · {}{} · article {}",
            index + 1,
            row.score()
                .map(|score| format!("{score:.1}"))
                .unwrap_or_else(|| "—".into()),
            row.title,
            row.stage,
            reason,
            row.article_id
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Behind the paper (§15.1)
// ---------------------------------------------------------------------------

/// The `limit` highest-utility articles the run did not select, shaped for
/// the "Behind the paper" chapter: the same query as `explain --near-misses`.
pub async fn paper_near_misses(
    db: &Db,
    run_id: i64,
    limit: usize,
) -> Result<Vec<NearMiss>, sqlx::Error> {
    Ok(near_misses(db, run_id, limit)
        .await?
        .iter()
        .map(|row| {
            let signals = row.signals();
            let raw = |name: &str| signals.as_ref().and_then(|s| s.raw.get(name).copied());
            NearMiss {
                article_id: row.article_id,
                title: row.title.clone(),
                feed_title: row.feed_title.clone(),
                quality: raw("quality"),
                fit: raw("fit"),
                stage: row.stage.clone(),
                reason: row.excluded_reason.clone(),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// `stats` (§15.3)
// ---------------------------------------------------------------------------

/// `admitted_by[0]`: the retriever that admitted a pick (§11).
fn first_retriever(admitted_by: Option<&str>) -> String {
    admitted_by
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .and_then(|names| names.into_iter().next())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Rated picks by admitting retriever: how many were rated, up, down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpDown {
    pub rated: i64,
    pub up: i64,
    pub down: i64,
}

impl UpDown {
    /// `"NN% up"`, or `n/a` with nothing rated.
    pub fn ratio(&self) -> String {
        if self.rated > 0 {
            format!("{:.0}% up", 100.0 * self.up as f64 / self.rated as f64)
        } else {
            "n/a".to_string()
        }
    }
}

/// One finished, non-dry run as a point of the per-run series (dashboard
/// plan §9.1, §12).
#[derive(Debug, Clone, PartialEq)]
pub struct RunPoint {
    pub run_id: i64,
    pub date: String,
    pub started_at: String,
    pub status: String,
    pub cost_usd: f64,
    pub selected: i64,
    pub duration_secs: Option<i64>,
}

/// Everything `daily-epub stats` prints, as data (dashboard plan §12): the
/// CLI renders it with [`render_stats_text`], the stats page as tables and
/// sparklines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsData {
    pub days: i64,
    /// First UTC date of the window.
    pub since_date: String,
    /// UTC date of `now`.
    pub today: String,
    pub issues: i64,
    pub published: i64,
    /// Explicit ratings per label, label order; `cleared` included.
    pub ratings_by_label: Vec<(String, i64)>,
    /// Explicit ratings excluding `cleared`.
    pub total_ratings: i64,
    pub per_retriever: BTreeMap<String, UpDown>,
    pub exploration_admitted: i64,
    pub exploration_selected: i64,
    pub exploration_positive: i64,
    /// Provider → spend over the whole window.
    pub provider_totals: BTreeMap<String, f64>,
    /// UTC date → provider → spend.
    pub cost_by_day: BTreeMap<String, BTreeMap<String, f64>>,
    /// Seconds of every finished run in the window (dry runs included), for
    /// the mean generation time.
    pub durations: Vec<i64>,
    /// Finished non-dry runs in the window, oldest first.
    pub runs: Vec<RunPoint>,
    /// Issue date → picks, oldest first.
    pub selected_per_issue: Vec<(String, i64)>,
    /// Week (Monday, UTC) → label → explicit ratings.
    pub ratings_per_week: BTreeMap<String, BTreeMap<String, i64>>,
}

impl StatsData {
    /// `n` per issue with one decimal, or `n/a` without issues.
    pub fn per_issue(&self, n: i64) -> String {
        if self.issues > 0 {
            format!("{:.1}", n as f64 / self.issues as f64)
        } else {
            "n/a".to_string()
        }
    }

    pub fn mean_issue_size(&self) -> String {
        self.per_issue(self.published)
    }

    pub fn ratings_per_issue(&self) -> String {
        self.per_issue(self.total_ratings)
    }

    /// Spend per day over the window for one provider total.
    pub fn per_day(&self, total: f64) -> f64 {
        total / self.days as f64
    }

    /// Every provider's window spend added up, in provider order.
    pub fn grand_total(&self) -> f64 {
        let mut grand = 0.0;
        for total in self.provider_totals.values() {
            grand += total;
        }
        grand
    }

    /// Integer mean of the finished runs' seconds.
    pub fn mean_generation_secs(&self) -> Option<i64> {
        if self.durations.is_empty() {
            None
        } else {
            Some(self.durations.iter().sum::<i64>() / self.durations.len() as i64)
        }
    }
}

/// `daily-epub stats [--days N]` as text: the whole evaluation framework
/// (§15.3). One fact per line, nothing wider than 80 columns.
pub async fn stats(db: &Db, days: i64, now: Timestamp) -> anyhow::Result<String> {
    Ok(render_stats_text(&stats_data(db, days, now).await?))
}

/// Finished non-dry runs as sparkline points, oldest first: those started
/// at or after `since` (RFC3339) and/or the newest `limit`.
pub async fn run_series(
    db: &Db,
    since: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<RunPoint>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, date, started_at, finished_at, status, cost_usd, selected FROM runs
         WHERE status != 'dry_run' AND finished_at IS NOT NULL
           AND (? IS NULL OR started_at >= ?)
         ORDER BY id DESC LIMIT ?",
    )
    .bind(since)
    .bind(since)
    .bind(limit.unwrap_or(i64::MAX))
    .fetch_all(db.pool())
    .await?;
    let mut points: Vec<RunPoint> = rows
        .iter()
        .map(|row| {
            let started_at: String = row.get("started_at");
            let finished_at: Option<String> = row.get("finished_at");
            let duration_secs = match (
                started_at.parse::<Timestamp>(),
                finished_at.as_deref().map(str::parse::<Timestamp>),
            ) {
                (Ok(started), Some(Ok(finished))) => {
                    Some((finished.as_second() - started.as_second()).max(0))
                }
                _ => None,
            };
            RunPoint {
                run_id: row.get("id"),
                date: row.get("date"),
                started_at,
                status: row.get("status"),
                cost_usd: row.get("cost_usd"),
                selected: row.get("selected"),
                duration_secs,
            }
        })
        .collect();
    points.reverse();
    Ok(points)
}

/// The Monday (UTC) of the week containing `ts`, as `YYYY-MM-DD`.
pub fn week_start(ts: Timestamp) -> String {
    let date = ts.to_zoned(jiff::tz::TimeZone::UTC).date();
    let offset = i64::from(date.weekday().to_monday_zero_offset());
    date.checked_sub(jiff::Span::new().days(offset))
        .unwrap_or(date)
        .to_string()
}

/// Gather every figure of [`StatsData`] for the last `days` days.
pub async fn stats_data(db: &Db, days: i64, now: Timestamp) -> anyhow::Result<StatsData> {
    let days = days.max(1);
    let since_ts = now
        .checked_sub(jiff::Span::new().hours(days.saturating_mul(24)))
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let since = fmt_ts(since_ts);
    let utc = jiff::tz::TimeZone::UTC;
    let since_date = since_ts.to_zoned(utc.clone()).date().to_string();
    let today = now.to_zoned(utc.clone()).date().to_string();
    let mut data = StatsData {
        days,
        since_date: since_date.clone(),
        today,
        ..StatsData::default()
    };

    // --- issues and articles ---
    data.issues = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE date >= ?")
        .bind(&since_date)
        .fetch_one(db.pool())
        .await?;
    data.published =
        sqlx::query_scalar("SELECT COUNT(*) FROM issue_articles WHERE issue_date >= ?")
            .bind(&since_date)
            .fetch_one(db.pool())
            .await?;
    let per_issue_rows = sqlx::query(
        "SELECT issue_date, COUNT(*) AS n FROM issue_articles
         WHERE issue_date >= ? GROUP BY issue_date ORDER BY issue_date",
    )
    .bind(&since_date)
    .fetch_all(db.pool())
    .await?;
    data.selected_per_issue = per_issue_rows
        .iter()
        .map(|row| (row.get::<String, _>("issue_date"), row.get::<i64, _>("n")))
        .collect();

    // --- explicit ratings by label ---
    let labels = sqlx::query(
        "SELECT label, COUNT(*) AS n FROM rating_events
         WHERE kind = 'explicit' AND event_at >= ? GROUP BY label ORDER BY label",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    for row in &labels {
        let label = row.get::<String, _>("label");
        let n = row.get::<i64, _>("n");
        if label != "cleared" {
            data.total_ratings += n;
        }
        data.ratings_by_label.push((label, n));
    }
    let events = sqlx::query(
        "SELECT label, event_at FROM rating_events
         WHERE kind = 'explicit' AND event_at >= ? ORDER BY event_at",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    for row in &events {
        let Ok(event_at) = row.get::<String, _>("event_at").parse::<Timestamp>() else {
            continue;
        };
        *data
            .ratings_per_week
            .entry(week_start(event_at))
            .or_default()
            .entry(row.get::<String, _>("label"))
            .or_insert(0) += 1;
    }

    // --- up/down per admitting retriever, from rated picks ---
    let rated_picks = sqlx::query(
        "WITH latest AS (
             SELECT re.article_id, re.issue_date, re.label, re.value,
                    ROW_NUMBER() OVER (
                        PARTITION BY re.article_id
                        ORDER BY re.event_at DESC, re.id DESC
                    ) AS rn
             FROM rating_events re
             WHERE re.kind = 'explicit' AND re.event_at >= ?
         )
         SELECT l.article_id, l.value, cr.admitted_by, cr.signals_json
         FROM latest l
         JOIN candidate_runs cr ON cr.article_id = l.article_id AND cr.stage = 'selected'
         JOIN runs r ON r.id = cr.run_id AND r.status != 'dry_run'
         WHERE l.rn = 1 AND l.label != 'cleared'
           AND (l.issue_date IS NULL OR r.date = l.issue_date)
         ORDER BY l.article_id, cr.run_id DESC",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    let mut seen: Option<ArticleId> = None;
    for row in &rated_picks {
        let article_id = row.get::<ArticleId, _>("article_id");
        if seen == Some(article_id) {
            continue; // a rerun of the date: keep the latest run only
        }
        seen = Some(article_id);
        let value = row.get::<f64, _>("value");
        let retriever = first_retriever(row.get::<Option<String>, _>("admitted_by").as_deref());
        let entry = data.per_retriever.entry(retriever).or_default();
        entry.rated += 1;
        if value > 0.0 {
            entry.up += 1;
        } else if value < 0.0 {
            entry.down += 1;
        }
        let exploration =
            serde_json::from_str::<SignalsJson>(&row.get::<String, _>("signals_json"))
                .map(|signals| signals.exploration)
                .unwrap_or(false);
        if exploration && value > 0.0 {
            data.exploration_positive += 1;
        }
    }

    // --- exploration yield ---
    let exploration_rows = sqlx::query(
        "SELECT cr.stage FROM candidate_runs cr
         JOIN runs r ON r.id = cr.run_id
         WHERE r.status != 'dry_run' AND r.started_at >= ?
           AND cr.signals_json LIKE '%\"exploration\":true%'",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    for row in &exploration_rows {
        match row.get::<String, _>("stage").as_str() {
            "selected" => {
                data.exploration_admitted += 1;
                data.exploration_selected += 1;
            }
            "admitted" | "assessed" | "shortlisted" => data.exploration_admitted += 1,
            _ => {}
        }
    }

    // --- cost per day per provider (§7.6) ---
    let cost_rows = sqlx::query(
        "SELECT started_at, provider_costs_json FROM runs
         WHERE started_at >= ? AND provider_costs_json IS NOT NULL",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    for row in &cost_rows {
        let raw = row.get::<String, _>("provider_costs_json");
        let Ok(providers) =
            serde_json::from_str::<BTreeMap<String, crate::report::ProviderUsage>>(&raw)
        else {
            continue;
        };
        let day = row
            .get::<String, _>("started_at")
            .parse::<Timestamp>()
            .map(|ts| ts.to_zoned(utc.clone()).date().to_string())
            .unwrap_or_else(|_| since_date.clone());
        for (provider, usage) in providers {
            *data.provider_totals.entry(provider.clone()).or_insert(0.0) += usage.cost_usd;
            *data
                .cost_by_day
                .entry(day.clone())
                .or_default()
                .entry(provider)
                .or_insert(0.0) += usage.cost_usd;
        }
    }

    // --- mean generation time ---
    let run_rows = sqlx::query(
        "SELECT started_at, finished_at FROM runs
         WHERE started_at >= ? AND finished_at IS NOT NULL",
    )
    .bind(&since)
    .fetch_all(db.pool())
    .await?;
    for row in &run_rows {
        let started = row.get::<String, _>("started_at").parse::<Timestamp>();
        let finished = row.get::<String, _>("finished_at").parse::<Timestamp>();
        if let (Ok(started), Ok(finished)) = (started, finished) {
            data.durations
                .push((finished.as_second() - started.as_second()).max(0));
        }
    }

    data.runs = run_series(db, Some(&since), None).await?;
    Ok(data)
}

/// The CLI text of [`StatsData`], one fact per line.
pub fn render_stats_text(data: &StatsData) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "stats: last {} days ({} → {})",
        data.days, data.since_date, data.today
    );
    let _ = writeln!(out, "issues: {}", data.issues);
    let _ = writeln!(out, "articles published: {}", data.published);
    let _ = writeln!(out, "mean issue size: {} articles", data.mean_issue_size());
    let _ = writeln!(out, "explicit ratings: {}", data.total_ratings);
    for (label, n) in &data.ratings_by_label {
        let _ = writeln!(out, "explicit ratings ({label}): {n}");
    }
    let _ = writeln!(out, "ratings per issue: {}", data.ratings_per_issue());
    if data.per_retriever.is_empty() {
        let _ = writeln!(out, "rated picks by admitting retriever: none");
    }
    for (retriever, counts) in &data.per_retriever {
        let _ = writeln!(
            out,
            "admitted by {retriever}: {} rated · {} up · {} down · {}",
            counts.rated,
            counts.up,
            counts.down,
            counts.ratio()
        );
    }
    let _ = writeln!(out, "exploration admitted: {}", data.exploration_admitted);
    let _ = writeln!(out, "exploration selected: {}", data.exploration_selected);
    let _ = writeln!(
        out,
        "exploration rated positively: {}",
        data.exploration_positive
    );
    for (provider, total) in &data.provider_totals {
        let _ = writeln!(
            out,
            "cost per day ({provider}): ${:.3}",
            data.per_day(*total)
        );
    }
    let _ = writeln!(
        out,
        "cost per day (total): ${:.3}",
        data.per_day(data.grand_total())
    );
    match data.mean_generation_secs() {
        None => {
            let _ = writeln!(out, "mean generation time: n/a (0 runs)");
        }
        Some(mean) => {
            let _ = writeln!(
                out,
                "mean generation time: {} ({} runs)",
                RunReport::format_duration(mean),
                data.durations.len()
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// `features prune` (§7.1, §7.4)
// ---------------------------------------------------------------------------

/// Rows removed by one [`prune`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    /// `article_embeddings` of unrated, unpublished articles past
    /// `embedding_retention_days`.
    pub embeddings: u64,
    /// `candidate_runs` rows of runs past `telemetry_retention_days`.
    pub telemetry: u64,
    /// `article_assessments` assessed more than `telemetry_retention_days` ago.
    pub assessments: u64,
}

/// Delete `article_embeddings` for articles neither rated nor published that
/// are older than `embedding_retention_days`, `candidate_runs` rows whose run
/// started more than `telemetry_retention_days` ago, and `article_assessments`
/// older than the same window (§7.1, §7.4). Runs from `features prune` and
/// once per `generate` after publishing.
pub async fn prune(
    db: &Db,
    embedding_retention_days: i64,
    telemetry_retention_days: i64,
    now: Timestamp,
) -> Result<Pruned, sqlx::Error> {
    let cutoff = |days: i64| {
        now.checked_sub(jiff::Span::new().hours(days.max(0).saturating_mul(24)))
            .unwrap_or(Timestamp::UNIX_EPOCH)
    };
    let embeddings = sqlx::query(
        "DELETE FROM article_embeddings
         WHERE article_id IN (
             SELECT ae.article_id
             FROM article_embeddings ae JOIN articles a ON a.id = ae.article_id
             WHERE a.first_seen < ?
               AND NOT EXISTS (SELECT 1 FROM rating_events re WHERE re.article_id = ae.article_id)
               AND NOT EXISTS (SELECT 1 FROM issue_articles ia WHERE ia.article_id = ae.article_id)
         )",
    )
    .bind(fmt_ts(cutoff(embedding_retention_days)))
    .execute(db.pool())
    .await?
    .rows_affected();

    let telemetry = sqlx::query(
        "DELETE FROM candidate_runs
         WHERE run_id IN (SELECT id FROM runs WHERE started_at < ?)",
    )
    .bind(fmt_ts(cutoff(telemetry_retention_days)))
    .execute(db.pool())
    .await?
    .rows_affected();

    let assessments = sqlx::query("DELETE FROM article_assessments WHERE assessed_at < ?")
        .bind(fmt_ts(cutoff(telemetry_retention_days)))
        .execute(db.pool())
        .await?
        .rows_affected();
    Ok(Pruned {
        embeddings,
        telemetry,
        assessments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curate::embedding::encode_blob;

    async fn db_with_articles(ids: &[ArticleId]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("telemetry.db"))
            .await
            .unwrap();
        for id in ids {
            sqlx::query(
                "INSERT INTO articles (id, canonical_url, title, first_seen)
                 VALUES (?, ?, ?, '2026-08-15T00:00:00Z')",
            )
            .bind(id)
            .bind(format!("https://example.com/{id}"))
            .bind(format!("Article {id}"))
            .execute(db.pool())
            .await
            .unwrap();
        }
        (dir, db)
    }

    fn date() -> Date {
        "2026-09-02".parse().unwrap()
    }

    fn signals(heuristic: f64, norm: f64) -> Signals {
        Signals {
            interest: Some(1.2),
            interest_top1_cos: Some(0.61),
            heuristic: Some(heuristic),
            norm: BTreeMap::from([("heuristic".into(), norm), ("interest".into(), 0.9)]),
            weights: BTreeMap::from([
                ("heuristic".into(), 0.2 / 0.55),
                ("interest".into(), 0.35 / 0.55),
            ]),
            top_interests: vec![TopInterest {
                name: "Gaussian Splatting".into(),
                z: 3.4,
                cos: 0.61,
            }],
            neighbours: vec![Neighbour {
                article_id: 812,
                label: "loved".into(),
                cos: 0.71,
                title: "A rated piece".into(),
            }],
            notes: vec!["knn gate 0.60 (n=14 rated with embeddings)".into()],
            ..Signals::default()
        }
    }

    #[test]
    fn signals_json_follows_the_plan_shape() {
        let json = serialize_signals(&signals(41.0, 0.55), false);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["raw"]["heuristic"], 41.0);
        assert_eq!(parsed["raw"]["interest_top1_cos"], 0.61);
        assert_eq!(parsed["present"]["heuristic"], true);
        assert_eq!(parsed["present"]["knn"], false);
        assert_eq!(parsed["present"]["quality"], false);
        assert!(parsed["raw"].get("knn").is_none());
        assert!(parsed["norm"].get("knn").is_none());
        assert_eq!(parsed["exploration"], false);
        assert_eq!(parsed["auto_include"], false);
        assert_eq!(parsed["top_interests"][0]["name"], "Gaussian Splatting");
        assert_eq!(parsed["neighbours"][0]["article_id"], 812);
        assert_eq!(
            parsed["notes"][0],
            "knn gate 0.60 (n=14 rated with embeddings)"
        );
        let typed: SignalsJson = serde_json::from_str(&json).unwrap();
        let weights: f64 = typed.weights.values().sum();
        assert!((weights - 1.0).abs() < 1e-9);
        assert!(typed.blend().is_some());
    }

    #[test]
    fn candidate_json_adds_triage_and_exploration() {
        let mut candidate = crate::types::Candidate::new(
            crate::curate::prefilter::tests::article(1, "Article", 900),
            false,
        );
        candidate.signals = signals(41.0, 0.55);
        candidate.exploration = true;
        candidate.assessment.triage = Some(crate::types::Triage {
            interest: 7.5,
            kind: "essay".into(),
            why: "specific".into(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T05:30:00Z".parse().unwrap(),
        });
        let parsed: serde_json::Value =
            serde_json::from_str(&serialize_candidate(&candidate)).unwrap();
        assert_eq!(parsed["raw"]["triage"], 7.5);
        assert_eq!(parsed["norm"]["triage"], 0.75);
        assert_eq!(parsed["present"]["triage"], true);
        assert_eq!(parsed["exploration"], true);
    }

    #[tokio::test]
    async fn explain_names_provider_rejections_instead_of_scores() {
        let (_dir, db) = db_with_articles(&[1]).await;
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 1,
                stage: "admitted",
                excluded_reason: None,
                admitted_by: Some("[\"interest\"]"),
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
             (article_id, stage, model, prompt_version, score, fit, kind, rationale, assessed_at)
             VALUES (1, 'triage', 'deepseek-v4-flash', 1, NULL, NULL, 'provider_rejected',
                     'deepseek: 400 Bad Request: Content Exists Risk', '2026-09-02T04:00:00Z'),
                    (1, 'deep', 'deepseek-v4-flash', 1, NULL, NULL, 'provider_rejected',
                     'deepseek: returned a refusal', '2026-09-02T04:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let text = explain(&db, date(), None, &ExplainTarget::Article(1))
            .await
            .unwrap();
        assert!(
            text.contains(
                "  triage: rejected by provider — deepseek: 400 Bad Request: Content Exists Risk\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  deep: rejected by provider — deepseek: returned a refusal\n"),
            "{text}"
        );
        assert!(!text.contains("interest —"), "{text}");
        assert!(!text.contains("quality —"), "{text}");
    }

    #[tokio::test]
    async fn rows_are_upserted_with_every_column_replaced() {
        let (_dir, db) = db_with_articles(&[1]).await;
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 1,
                stage: "eligible",
                excluded_reason: Some("not_admitted"),
                admitted_by: None,
                signals_json: "{}",
                utility: Some(1.0),
                rank_utility: None,
                cluster_id: None,
                cluster_rank: None,
                editor_why: None,
            },
        )
        .await
        .unwrap();
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 1,
                stage: "selected",
                excluded_reason: None,
                admitted_by: Some("[\"prefilter\"]"),
                signals_json: "{\"v\":1}",
                utility: None,
                rank_utility: None,
                cluster_id: None,
                cluster_rank: None,
                editor_why: Some("because"),
            },
        )
        .await
        .unwrap();
        let row = explain_row(&db, run_id, 1).await.unwrap().unwrap();
        assert_eq!(row.stage, "selected");
        assert_eq!(row.excluded_reason, None, "no COALESCE");
        assert_eq!(row.utility, None);
        assert_eq!(row.admitted_by.as_deref(), Some("[\"prefilter\"]"));
        assert_eq!(row.editor_why.as_deref(), Some("because"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candidate_runs")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn explain_renders_persisted_rows_and_reports_never_ingested() {
        let (_dir, db) = db_with_articles(&[1, 2]).await;
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        thin_excluded(&db, run_id, 2, "blocked").await.unwrap();
        let json = serialize_signals(&signals(41.0, 0.55), true);
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 1,
                stage: "shortlisted",
                excluded_reason: Some("not_selected"),
                admitted_by: Some("[\"prefilter\"]"),
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
        // A later dry run must not shadow the real one.
        let dry = db.start_run(date(), Timestamp::now()).await.unwrap();
        sqlx::query("UPDATE runs SET status = 'dry_run' WHERE id = ?")
            .bind(dry)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(resolve_run(&db, date(), None).await.unwrap(), Some(run_id));
        assert_eq!(
            resolve_run(&db, date(), Some(dry)).await.unwrap(),
            Some(dry)
        );
        assert_eq!(
            resolve_run(&db, "2026-01-01".parse().unwrap(), Some(dry))
                .await
                .unwrap(),
            None
        );

        let text = explain(&db, date(), None, &ExplainTarget::Article(1))
            .await
            .unwrap();
        assert!(text.contains("article 1: Article 1"), "{text}");
        assert!(
            text.contains("stage: shortlisted · reason: not_selected"),
            "{text}"
        );
        let squashed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squashed.contains("heuristic 41.000 · 0.550 · 0.364"),
            "{text}"
        );
        assert!(squashed.contains("knn absent"), "{text}");
        assert!(squashed.contains("quality absent"), "{text}");
        assert!(
            text.contains("Gaussian Splatting · z 3.40 · cos 0.610"),
            "{text}"
        );
        assert!(
            text.contains("loved · cos 0.710 · article 812 · A rated piece"),
            "{text}"
        );
        assert!(text.contains("admitted by: [\"prefilter\"]"), "{text}");
        assert!(text.contains("auto_include=true"), "{text}");
        assert!(text.contains("preliminary blend:"), "{text}");
        assert!(text.contains("note: knn gate"), "{text}");

        let by_url = explain(
            &db,
            date(),
            None,
            &ExplainTarget::Url("https://example.com/1?utm_source=x".into()),
        )
        .await
        .unwrap();
        assert_eq!(by_url, text, "--url canonicalizes and finds the same row");

        let thin = explain(&db, date(), None, &ExplainTarget::Article(2))
            .await
            .unwrap();
        assert!(thin.contains("stage: excluded · reason: blocked"), "{thin}");

        let missing = explain(
            &db,
            date(),
            None,
            &ExplainTarget::Url("https://nowhere.example/post".into()),
        )
        .await
        .unwrap();
        assert!(missing.contains("never ingested"), "{missing}");
        let missing_id = explain(&db, date(), None, &ExplainTarget::Article(99))
            .await
            .unwrap();
        assert!(missing_id.contains("never ingested"), "{missing_id}");

        let no_run = explain(
            &db,
            "2026-01-01".parse().unwrap(),
            None,
            &ExplainTarget::Article(1),
        )
        .await
        .unwrap();
        assert!(no_run.contains("no non-dry run"), "{no_run}");
        let not_considered = explain(&db, date(), Some(dry), &ExplainTarget::Article(1))
            .await
            .unwrap();
        assert!(
            not_considered.contains("was not considered by run"),
            "{not_considered}"
        );
    }

    #[tokio::test]
    async fn near_misses_rank_by_utility_then_blend_and_skip_selected_and_excluded() {
        let (_dir, db) = db_with_articles(&[1, 2, 3, 4, 5, 6]).await;
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        // Utility decides wherever the ranker wrote one; the preliminary blend
        // only stands in for rows the deep set never reached. Article 4's blend
        // would put it first, but its utility is the lowest; article 3 never
        // got a utility and ranks on its blend.
        let rows = [
            (1, "selected", None, 0.9, Some(90.0)),
            (2, "shortlisted", Some("not_selected"), 0.1, Some(70.0)),
            (3, "eligible", Some("not_admitted"), 0.95, None),
            (4, "shortlisted", Some("not_selected"), 0.99, Some(10.0)),
            (6, "assessed", Some("cluster_suppressed"), 0.5, Some(40.0)),
        ];
        for (id, stage, reason, norm, utility) in rows {
            let json = serialize_signals(&signals(10.0, norm), false);
            write(
                &db,
                &CandidateRun {
                    run_id,
                    article_id: id,
                    stage,
                    excluded_reason: reason,
                    admitted_by: None,
                    signals_json: &json,
                    utility,
                    rank_utility: None,
                    cluster_id: None,
                    cluster_rank: None,
                    editor_why: None,
                },
            )
            .await
            .unwrap();
        }
        thin_excluded(&db, run_id, 5, "published_before")
            .await
            .unwrap();
        let misses = near_misses(&db, run_id, 10).await.unwrap();
        assert_eq!(
            misses.iter().map(|row| row.article_id).collect::<Vec<_>>(),
            vec![3, 2, 6, 4]
        );
        let text = explain_near_misses(&db, date(), None, 2).await.unwrap();
        assert!(text.contains("top 2 not selected, by utility"), "{text}");
        assert!(
            text.contains("Article 3 · eligible, not_admitted"),
            "{text}"
        );
        assert!(
            text.contains("Article 2 · shortlisted, not_selected"),
            "{text}"
        );
        assert!(!text.contains("Article 4"), "{text}");

        // Without any utility the listing says so and orders by the blend.
        sqlx::query("UPDATE candidate_runs SET utility = NULL WHERE run_id = ?")
            .bind(run_id)
            .execute(db.pool())
            .await
            .unwrap();
        let misses = near_misses(&db, run_id, 10).await.unwrap();
        assert_eq!(
            misses.iter().map(|row| row.article_id).collect::<Vec<_>>(),
            vec![4, 3, 6, 2]
        );
        let text = explain_near_misses(&db, date(), None, 1).await.unwrap();
        assert!(
            text.contains("top 1 not selected, by preliminary blend"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn paper_near_misses_carry_feed_quality_fit_and_stage() {
        let (_dir, db) = db_with_articles(&[1, 2]).await;
        sqlx::query(
            "INSERT INTO entries (id, feed_id, feed_title, title, url, raw_content, fetched_at)
             VALUES (11, 5, 'Example Feed', 'Article 1', 'https://example.com/1', '', '2026-08-15T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE articles SET best_entry_id = 11 WHERE id = 1")
            .execute(db.pool())
            .await
            .unwrap();
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        let mut candidate = crate::types::Candidate::new(
            crate::curate::prefilter::tests::article(1, "Article 1", 900),
            false,
        );
        candidate.signals = signals(41.0, 0.55);
        candidate.assessment.deep = Some(crate::types::Deep {
            quality: 8.0,
            fit: 6.5,
            category: None,
            rationale: String::new(),
            paywalled_guess: false,
            facets: Default::default(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T05:30:00Z".parse().unwrap(),
        });
        let json = serialize_candidate(&candidate);
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 1,
                stage: "shortlisted",
                excluded_reason: Some("not_selected"),
                admitted_by: Some("[\"triage\"]"),
                signals_json: &json,
                utility: Some(71.0),
                rank_utility: Some(3),
                cluster_id: None,
                cluster_rank: None,
                editor_why: None,
            },
        )
        .await
        .unwrap();
        write(
            &db,
            &CandidateRun {
                run_id,
                article_id: 2,
                stage: "selected",
                excluded_reason: None,
                admitted_by: Some("[\"triage\"]"),
                signals_json: "{}",
                utility: Some(90.0),
                rank_utility: Some(1),
                cluster_id: None,
                cluster_rank: None,
                editor_why: Some("because"),
            },
        )
        .await
        .unwrap();
        let misses = paper_near_misses(&db, run_id, 10).await.unwrap();
        assert_eq!(misses.len(), 1, "selected picks are not near misses");
        let miss = &misses[0];
        assert_eq!(miss.article_id, 1);
        assert_eq!(miss.title, "Article 1");
        assert_eq!(miss.feed_title, "Example Feed");
        assert_eq!(miss.quality, Some(8.0));
        assert_eq!(miss.fit, Some(6.5));
        assert_eq!(miss.stage, "shortlisted");
        assert_eq!(miss.reason.as_deref(), Some("not_selected"));
    }

    #[tokio::test]
    async fn stats_prints_every_fact_from_runs_issues_and_ratings() {
        let (_dir, db) = db_with_articles(&[1, 2, 3, 4]).await;
        let now: Timestamp = "2026-09-02T12:00:00Z".parse().unwrap();
        // Two issues inside the window, one outside it.
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at) VALUES
                 ('2026-08-01', 1, '2026-08-01T10:00:00Z'),
                 ('2026-08-30', 30, '2026-08-30T10:00:00Z'),
                 ('2026-09-01', 32, '2026-09-01T10:00:00Z');
             INSERT INTO issue_articles (issue_date, article_id, section) VALUES
                 ('2026-08-01', 4, 'Top Stories'),
                 ('2026-08-30', 1, 'Top Stories'),
                 ('2026-08-30', 2, 'Top Stories'),
                 ('2026-09-01', 3, 'Top Stories');",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Two finished runs with provider costs, one of them a rerun of 08-30.
        let mut run_ids = Vec::new();
        for (date, started, finished, costs) in [
            (
                "2026-08-30",
                "2026-08-30T09:30:00Z",
                "2026-08-30T09:50:00Z",
                r#"{"deepseek":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":1,"cost_usd":0.10},"anthropic":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":1,"cost_usd":0.60},"voyage":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":0,"cost_usd":0.02}}"#,
            ),
            (
                "2026-08-30",
                "2026-08-30T11:00:00Z",
                "2026-08-30T11:10:00Z",
                r#"{"deepseek":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":1,"cost_usd":0.04}}"#,
            ),
            (
                "2026-09-01",
                "2026-09-01T09:30:00Z",
                "2026-09-01T09:45:00Z",
                r#"{"deepseek":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":1,"cost_usd":0.14},"gemini":{"input_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"output_tokens":1,"cost_usd":0.07}}"#,
            ),
        ] {
            let run_id = db
                .start_run(date.parse().unwrap(), started.parse().unwrap())
                .await
                .unwrap();
            sqlx::query(
                "UPDATE runs SET finished_at = ?, status = 'ok', provider_costs_json = ? WHERE id = ?",
            )
            .bind(finished)
            .bind(costs)
            .bind(run_id)
            .execute(db.pool())
            .await
            .unwrap();
            run_ids.push(run_id);
        }
        // Article 1 was admitted by triage in the first 08-30 run and by knn
        // in the rerun; the latest run wins. Article 2 was an exploration
        // pick admitted by exploration. Article 3 was admitted by blend.
        let exploration = r#"{"v":1,"exploration":true}"#;
        for (run_id, article_id, stage, admitted_by, json) in [
            (run_ids[0], 1, "selected", "[\"triage\"]", "{}"),
            (run_ids[1], 1, "selected", "[\"knn\"]", "{}"),
            (run_ids[1], 2, "selected", "[\"exploration\"]", exploration),
            (run_ids[1], 4, "assessed", "[\"exploration\"]", exploration),
            (run_ids[2], 3, "selected", "[\"blend\"]", "{}"),
        ] {
            write(
                &db,
                &CandidateRun {
                    run_id,
                    article_id,
                    stage,
                    excluded_reason: None,
                    admitted_by: Some(admitted_by),
                    signals_json: json,
                    utility: Some(50.0),
                    rank_utility: None,
                    cluster_id: None,
                    cluster_rank: None,
                    editor_why: None,
                },
            )
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO rating_events (article_id, issue_date, kind, source, label, value, event_at) VALUES
                 (1, '2026-08-30', 'explicit', 'epub', 'good', 0.35, '2026-08-31T08:00:00Z'),
                 (1, '2026-08-30', 'explicit', 'epub', 'loved', 1.0, '2026-08-31T09:00:00Z'),
                 (2, '2026-08-30', 'explicit', 'epub', 'loved', 1.0, '2026-08-31T09:30:00Z'),
                 (3, '2026-09-01', 'explicit', 'epub', 'not_for_me', -1.0, '2026-09-01T12:00:00Z'),
                 (4, '2026-08-01', 'explicit', 'epub', 'loved', 1.0, '2026-08-02T12:00:00Z'),
                 (3, NULL, 'explicit', 'cli', 'cleared', 0.0, '2026-08-20T12:00:00Z');",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let text = stats(&db, 14, now).await.unwrap();
        println!("{text}");
        for line in [
            "stats: last 14 days (2026-08-19 → 2026-09-02)",
            "issues: 2",
            "articles published: 3",
            "mean issue size: 1.5 articles",
            "explicit ratings: 4",
            "explicit ratings (cleared): 1",
            "explicit ratings (good): 1",
            "explicit ratings (loved): 2",
            "explicit ratings (not_for_me): 1",
            "ratings per issue: 2.0",
            "admitted by blend: 1 rated · 0 up · 1 down · 0% up",
            "admitted by exploration: 1 rated · 1 up · 0 down · 100% up",
            "admitted by knn: 1 rated · 1 up · 0 down · 100% up",
            "exploration admitted: 2",
            "exploration selected: 1",
            "exploration rated positively: 1",
            "cost per day (anthropic): $0.043",
            "cost per day (deepseek): $0.020",
            "cost per day (gemini): $0.005",
            "cost per day (voyage): $0.001",
            "cost per day (total): $0.069",
            "mean generation time: 15m00s (3 runs)",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        assert!(
            !text.contains("admitted by triage"),
            "the rerun's row replaces the first run's: {text}"
        );
        assert!(
            text.lines().all(|line| line.chars().count() <= 80),
            "no line wider than 80 columns"
        );
        // Dashboard plan §12: the CLI output is byte-identical before and
        // after the `stats_data` / `render_stats_text` split.
        assert_eq!(text, STATS_TEXT_BEFORE_REFACTOR);
        let data = stats_data(&db, 14, now).await.unwrap();
        assert_eq!(render_stats_text(&data), text);

        // The figures the stats page adds on top of the text.
        assert_eq!(data.issues, 2);
        assert_eq!(data.total_ratings, 4);
        assert_eq!(
            data.selected_per_issue,
            vec![("2026-08-30".to_string(), 2), ("2026-09-01".to_string(), 1)]
        );
        assert_eq!(
            data.cost_by_day["2026-08-30"]["deepseek"],
            0.10 + 0.04,
            "both 08-30 runs land on the same day"
        );
        assert_eq!(data.cost_by_day["2026-09-01"]["gemini"], 0.07);
        assert_eq!(data.ratings_per_week["2026-08-31"]["loved"], 2);
        assert_eq!(data.ratings_per_week["2026-08-31"]["not_for_me"], 1);
        assert_eq!(data.ratings_per_week["2026-08-17"]["cleared"], 1);
        assert_eq!(data.runs.len(), 3, "three finished non-dry runs");
        assert_eq!(data.runs[0].run_id, run_ids[0], "oldest first");
        assert_eq!(data.runs[0].duration_secs, Some(1200));
        assert_eq!(data.runs[2].date, "2026-09-01");
        assert_eq!(data.per_retriever["knn"].ratio(), "100% up");
        assert_eq!(data.mean_generation_secs(), Some(900));

        // The newest-N form the overview uses.
        let last_two = run_series(&db, None, Some(2)).await.unwrap();
        assert_eq!(
            last_two.iter().map(|p| p.run_id).collect::<Vec<_>>(),
            vec![run_ids[1], run_ids[2]]
        );

        // An empty database still prints every heading.
        let (_dir, empty) = db_with_articles(&[]).await;
        let text = stats(&empty, 7, now).await.unwrap();
        for line in [
            "issues: 0",
            "mean issue size: n/a articles",
            "ratings per issue: n/a",
            "rated picks by admitting retriever: none",
            "cost per day (total): $0.000",
            "mean generation time: n/a (0 runs)",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        assert_eq!(text, STATS_TEXT_EMPTY_BEFORE_REFACTOR);
    }

    /// `stats(db, 14, 2026-09-02T12:00:00Z)` over the seed above, captured
    /// from the pre-refactor implementation.
    const STATS_TEXT_BEFORE_REFACTOR: &str = "\
stats: last 14 days (2026-08-19 → 2026-09-02)
issues: 2
articles published: 3
mean issue size: 1.5 articles
explicit ratings: 4
explicit ratings (cleared): 1
explicit ratings (good): 1
explicit ratings (loved): 2
explicit ratings (not_for_me): 1
ratings per issue: 2.0
admitted by blend: 1 rated · 0 up · 1 down · 0% up
admitted by exploration: 1 rated · 1 up · 0 down · 100% up
admitted by knn: 1 rated · 1 up · 0 down · 100% up
exploration admitted: 2
exploration selected: 1
exploration rated positively: 1
cost per day (anthropic): $0.043
cost per day (deepseek): $0.020
cost per day (gemini): $0.005
cost per day (voyage): $0.001
cost per day (total): $0.069
mean generation time: 15m00s (3 runs)
";

    const STATS_TEXT_EMPTY_BEFORE_REFACTOR: &str = "\
stats: last 7 days (2026-08-26 → 2026-09-02)
issues: 0
articles published: 0
mean issue size: n/a articles
explicit ratings: 0
ratings per issue: n/a
rated picks by admitting retriever: none
exploration admitted: 0
exploration selected: 0
exploration rated positively: 0
cost per day (total): $0.000
mean generation time: n/a (0 runs)
";

    #[test]
    fn week_start_is_the_utc_monday() {
        assert_eq!(
            week_start("2026-09-02T12:00:00Z".parse().unwrap()),
            "2026-08-31"
        );
        assert_eq!(
            week_start("2026-08-31T00:00:00Z".parse().unwrap()),
            "2026-08-31"
        );
        assert_eq!(
            week_start("2026-09-06T23:59:59Z".parse().unwrap()),
            "2026-08-31"
        );
        assert_eq!(
            week_start("2026-09-07T00:00:00Z".parse().unwrap()),
            "2026-09-07"
        );
    }

    #[tokio::test]
    async fn prune_respects_rated_and_published() {
        let (_dir, db) = db_with_articles(&[1, 2, 3, 4]).await;
        let now = Timestamp::now();
        let old = fmt_ts(now - jiff::Span::new().hours(200 * 24));
        sqlx::query("UPDATE articles SET first_seen = ? WHERE id IN (1, 2, 3)")
            .bind(&old)
            .execute(db.pool())
            .await
            .unwrap();
        let blob = encode_blob(&[0.5, 0.5]).unwrap();
        for id in 1..=4 {
            sqlx::query(
                "INSERT INTO article_embeddings
                     (article_id, model, dimension, input_hash, embedding, created_at)
                 VALUES (?, 'voyage-4-lite', 2, 'h', ?, ?)",
            )
            .bind(id)
            .bind(&blob)
            .bind(&old)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO rating_events (article_id, kind, source, label, value, event_at)
             VALUES (1, 'explicit', 'cli', 'loved', 1.0, ?)",
        )
        .bind(&old)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at) VALUES ('2026-02-01', 1, ?);
             INSERT INTO issue_articles (issue_date, article_id, section) VALUES ('2026-02-01', 2, 'Top Stories');",
        )
        .bind(&old)
        .execute(db.pool())
        .await
        .unwrap();

        let old_run = db
            .start_run("2026-02-01".parse().unwrap(), now)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET started_at = ? WHERE id = ?")
            .bind(&old)
            .bind(old_run)
            .execute(db.pool())
            .await
            .unwrap();
        let new_run = db.start_run(date(), now).await.unwrap();
        thin_excluded(&db, old_run, 1, "blocked").await.unwrap();
        thin_excluded(&db, new_run, 1, "blocked").await.unwrap();
        for (id, assessed_at) in [(1, old.clone()), (2, fmt_ts(now))] {
            sqlx::query(
                "INSERT INTO article_assessments
                     (article_id, stage, model, prompt_version, score, assessed_at)
                 VALUES (?, 'triage', 'deepseek-v4-flash', 1, 7.0, ?)",
            )
            .bind(id)
            .bind(&assessed_at)
            .execute(db.pool())
            .await
            .unwrap();
        }

        let pruned = prune(&db, 120, 180, now).await.unwrap();
        assert_eq!(
            pruned.embeddings, 1,
            "only the old, unrated, unpublished article 3"
        );
        assert_eq!(pruned.telemetry, 1, "only the old run's rows");
        assert_eq!(pruned.assessments, 1, "only the 200-day-old assessment");
        let remaining: Vec<i64> =
            sqlx::query_scalar("SELECT article_id FROM article_embeddings ORDER BY article_id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(remaining, vec![1, 2, 4]);
        let runs: Vec<i64> = sqlx::query_scalar("SELECT run_id FROM candidate_runs")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(runs, vec![new_run]);
        let assessed: Vec<i64> = sqlx::query_scalar("SELECT article_id FROM article_assessments")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(assessed, vec![2]);

        // A second pass finds nothing left to remove.
        assert_eq!(prune(&db, 120, 180, now).await.unwrap(), Pruned::default());
    }
}
