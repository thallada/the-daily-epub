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
use crate::types::ArticleId;

/// The stage vocabulary of §7.4, in pipeline order.
pub const STAGES: [&str; 7] = [
    "excluded",
    "eligible",
    "triaged",
    "admitted",
    "assessed",
    "shortlisted",
    "selected",
];

/// Signal names rendered by `explain`, including the LLM ones steps 4–5 add.
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

// ---------------------------------------------------------------------------
// `explain` (§15.2)
// ---------------------------------------------------------------------------

/// A `candidate_runs` row joined to its article title.
#[derive(Debug, Clone)]
pub struct ExplainRow {
    pub run_id: i64,
    pub article_id: ArticleId,
    pub title: String,
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

    /// Utility when step 5 has written it, else the preliminary blend.
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
                cr.stage, cr.excluded_reason, cr.admitted_by, cr.signals_json,
                cr.utility, cr.rank_utility, cr.cluster_id, cr.cluster_rank, cr.editor_why
         FROM candidate_runs cr JOIN articles a ON a.id = cr.article_id
         WHERE cr.run_id = ? AND cr.article_id = ?",
    )
    .bind(run_id)
    .bind(article_id)
    .fetch_optional(db.pool())
    .await?;
    Ok(row.as_ref().map(ExplainRow::from_row))
}

/// The top `limit` rows by utility-or-blend that were not selected (§15.2).
pub async fn near_misses(
    db: &Db,
    run_id: i64,
    limit: usize,
) -> Result<Vec<ExplainRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT cr.run_id, cr.article_id, COALESCE(a.title, '') AS title,
                cr.stage, cr.excluded_reason, cr.admitted_by, cr.signals_json,
                cr.utility, cr.rank_utility, cr.cluster_id, cr.cluster_rank, cr.editor_why
         FROM candidate_runs cr JOIN articles a ON a.id = cr.article_id
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
            let _ = writeln!(
                out,
                "  {} · {} · score {} · fit {} · kind {} · category {} · paywalled={} · {}",
                assessment.get::<String, _>("stage"),
                assessment.get::<String, _>("model"),
                fmt_opt(assessment.get::<Option<f64>, _>("score")),
                fmt_opt(assessment.get::<Option<f64>, _>("fit")),
                assessment
                    .get::<Option<String>, _>("kind")
                    .unwrap_or_else(|| "—".into()),
                assessment
                    .get::<Option<String>, _>("category")
                    .unwrap_or_else(|| "—".into()),
                assessment.get::<i64, _>("paywalled_guess") != 0,
                assessment
                    .get::<Option<String>, _>("rationale")
                    .unwrap_or_default(),
            );
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
// `features prune` (§7.1, §7.4)
// ---------------------------------------------------------------------------

/// Delete `article_embeddings` for articles neither rated nor published that
/// are older than `embedding_retention_days`, and `candidate_runs` rows whose
/// run started more than `telemetry_retention_days` ago. Returns the counts.
pub async fn prune(
    db: &Db,
    embedding_retention_days: i64,
    telemetry_retention_days: i64,
    now: Timestamp,
) -> Result<(u64, u64), sqlx::Error> {
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
    Ok((embeddings, telemetry))
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
    async fn near_misses_rank_by_blend_and_skip_selected_and_excluded() {
        let (_dir, db) = db_with_articles(&[1, 2, 3, 4, 5]).await;
        let run_id = db.start_run(date(), Timestamp::now()).await.unwrap();
        let rows = [
            (1, "selected", None, 0.9),
            (2, "shortlisted", Some("not_selected"), 0.7),
            (3, "eligible", Some("not_admitted"), 0.95),
            (4, "shortlisted", Some("not_selected"), 0.1),
        ];
        for (id, stage, reason, norm) in rows {
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
        thin_excluded(&db, run_id, 5, "published_before")
            .await
            .unwrap();
        let misses = near_misses(&db, run_id, 10).await.unwrap();
        assert_eq!(
            misses.iter().map(|row| row.article_id).collect::<Vec<_>>(),
            vec![3, 2, 4]
        );
        let text = explain_near_misses(&db, date(), None, 2).await.unwrap();
        assert!(
            text.contains("top 2 not selected, by preliminary blend"),
            "{text}"
        );
        assert!(
            text.contains("Article 3 · eligible, not_admitted"),
            "{text}"
        );
        assert!(!text.contains("Article 4"), "{text}");
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

        let (embeddings, telemetry) = prune(&db, 120, 180, now).await.unwrap();
        assert_eq!(
            embeddings, 1,
            "only the old, unrated, unpublished article 3"
        );
        assert_eq!(telemetry, 1, "only the old run's rows");
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
    }
}
