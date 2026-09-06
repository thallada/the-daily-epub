//! Dashboard: articles list and article detail (dashboard plan §9.3).
//!
//! The list joins every article to its best entry, its **latest**
//! `candidate_runs` row (via `idx_candidate_runs_article_run`), both
//! assessments, the current explicit rating and the latest publication. The
//! detail page shows everything the system knows about one article, in the
//! order of §9.3, compares its embedding with every compatible cached article,
//! and wraps `telemetry::render_explain` verbatim in `<pre>`.

use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use jiff::civil::Date;
use serde::Deserialize;
use sqlx::Row as _;

use super::{
    Bind, Pager, REASONS, STAGES, SignalsView, admitted_by_parts, allow_listed, bind_all, db_err,
    dynamic_query, fmt_opt, fmt_opt_int, fmt_stored_time, like_pattern, non_empty, page_number,
    widget_label,
};
use crate::config::Config;
use crate::curate::embedding::{self, decode_blob};
use crate::curate::telemetry;
use crate::curate::triage::{PROVIDER_REJECTED, TRIAGE_KINDS};
use crate::db::Db;
use crate::server::AppState;
use crate::types::ArticleId;
use crate::web::rate::RatingWidget;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, Pagination, WebError, take_flash};

const ARTICLES_PER_PAGE: u32 = 50;
const NEAREST_ARTICLES: usize = 10;
const CURRENT_RATING_LOOKBACK_DAYS: i64 = 36_500;

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/articles", get(list))
        .route("/dashboard/articles/{id}", get(detail))
}

// ---------------------------------------------------------------------------
// Articles list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ArticlesQuery {
    pub q: Option<String>,
    pub feed: Option<String>,
    pub stage: Option<String>,
    pub reason: Option<String>,
    pub rated: Option<String>,
    pub published: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: Option<String>,
    pub sort: Option<String>,
    pub page: Option<u32>,
}

const RATED: [&str; 6] = ["any", "loved", "good", "down", "cleared", "none"];
const PUBLISHED: [&str; 2] = ["yes", "no"];

const ARTICLE_SORTS: [(&str, &str); 7] = [
    ("first_seen", "x.first_seen DESC, x.id DESC"),
    ("utility", "x.utility DESC, x.id DESC"),
    ("quality", "x.quality DESC, x.id DESC"),
    ("fit", "x.fit DESC, x.id DESC"),
    ("triage", "x.triage DESC, x.id DESC"),
    ("words", "x.word_count DESC, x.id DESC"),
    ("title", "x.title COLLATE NOCASE ASC, x.id ASC"),
];

/// Validated article filters; see [`CandidateFilters`](super::runs::CandidateFilters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArticleFilters {
    pub q: Option<String>,
    pub feed: Option<i64>,
    pub stage: Option<String>,
    pub reason: Option<String>,
    pub rated: Option<String>,
    pub published: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub kind: Option<String>,
    pub sort: &'static str,
}

impl ArticleFilters {
    pub fn from_query(query: &ArticlesQuery) -> Self {
        let owned = |value: Option<&str>| value.map(str::to_string);
        let date = |value: Option<&str>| {
            non_empty(value)
                .and_then(|value| value.parse::<Date>().ok())
                .map(|date| date.to_string())
        };
        let mut kinds: Vec<&str> = TRIAGE_KINDS.to_vec();
        kinds.push(PROVIDER_REJECTED);
        Self {
            q: owned(non_empty(query.q.as_deref())),
            feed: non_empty(query.feed.as_deref()).and_then(|feed| feed.parse::<i64>().ok()),
            stage: owned(allow_listed(query.stage.as_deref(), &STAGES)),
            reason: owned(allow_listed(query.reason.as_deref(), &REASONS)),
            rated: owned(allow_listed(query.rated.as_deref(), &RATED)),
            published: owned(allow_listed(query.published.as_deref(), &PUBLISHED)),
            from: date(query.from.as_deref()),
            to: date(query.to.as_deref()),
            kind: owned(allow_listed(query.kind.as_deref(), &kinds)),
            sort: ARTICLE_SORTS
                .iter()
                .find(|(name, _)| Some(*name) == query.sort.as_deref())
                .map(|(name, _)| *name)
                .unwrap_or(ARTICLE_SORTS[0].0),
        }
    }

    fn order_by(&self) -> &'static str {
        ARTICLE_SORTS
            .iter()
            .find(|(name, _)| *name == self.sort)
            .map(|(_, sql)| *sql)
            .unwrap_or(ARTICLE_SORTS[0].1)
    }

    fn where_clauses(&self) -> (String, Vec<Bind>) {
        let mut sql = String::new();
        let mut binds = Vec::new();
        if let Some(q) = &self.q {
            sql.push_str(" AND (x.title LIKE ? ESCAPE '\\' OR x.canonical_url LIKE ? ESCAPE '\\')");
            binds.push(Bind::Text(like_pattern(q)));
            binds.push(Bind::Text(like_pattern(q)));
        }
        if let Some(feed) = self.feed {
            sql.push_str(" AND x.feed_id = ?");
            binds.push(Bind::Int(feed));
        }
        if let Some(stage) = &self.stage {
            sql.push_str(" AND x.stage = ?");
            binds.push(Bind::Text(stage.clone()));
        }
        if let Some(reason) = &self.reason {
            sql.push_str(" AND x.excluded_reason = ?");
            binds.push(Bind::Text(reason.clone()));
        }
        match self.rated.as_deref() {
            Some("any") => sql.push_str(" AND x.rating IS NOT NULL AND x.rating != 'cleared'"),
            Some("none") => sql.push_str(" AND x.rating IS NULL"),
            Some(label @ ("loved" | "good" | "cleared")) => {
                sql.push_str(" AND x.rating = ?");
                binds.push(Bind::Text(label.to_string()));
            }
            Some("down") => sql.push_str(" AND x.rating = 'not_for_me'"),
            _ => {}
        }
        match self.published.as_deref() {
            Some("yes") => sql.push_str(" AND x.published IS NOT NULL"),
            Some("no") => sql.push_str(" AND x.published IS NULL"),
            _ => {}
        }
        if let Some(from) = &self.from {
            sql.push_str(" AND substr(x.first_seen, 1, 10) >= ?");
            binds.push(Bind::Text(from.clone()));
        }
        if let Some(to) = &self.to {
            sql.push_str(" AND substr(x.first_seen, 1, 10) <= ?");
            binds.push(Bind::Text(to.clone()));
        }
        if let Some(kind) = &self.kind {
            sql.push_str(" AND x.triage_kind = ?");
            binds.push(Bind::Text(kind.clone()));
        }
        (sql, binds)
    }

    fn params(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("q", self.q.clone()),
            ("feed", self.feed.map(|feed| feed.to_string())),
            ("stage", self.stage.clone()),
            ("reason", self.reason.clone()),
            ("rated", self.rated.clone()),
            ("published", self.published.clone()),
            ("from", self.from.clone()),
            ("to", self.to.clone()),
            ("kind", self.kind.clone()),
            (
                "sort",
                (self.sort != ARTICLE_SORTS[0].0).then(|| self.sort.to_string()),
            ),
        ]
    }
}

/// One row of the articles table.
#[derive(Debug, Clone)]
pub struct ArticleListRow {
    pub id: ArticleId,
    pub href: String,
    pub title: String,
    pub feed_id: Option<i64>,
    pub feed: String,
    pub first_seen: String,
    pub words: i64,
    pub stage: Option<String>,
    pub reason: Option<String>,
    pub run_id: Option<i64>,
    pub run_date: Option<String>,
    pub utility: String,
    pub triage: String,
    pub quality: String,
    pub fit: String,
    pub rating: Option<String>,
    pub rating_class: &'static str,
    pub published: Option<String>,
}

const ARTICLE_INNER: &str =
    "SELECT a.id, COALESCE(a.title, '') AS title, a.canonical_url, a.first_seen,
                a.word_count, e.feed_id, COALESCE(e.feed_title, '') AS feed_title,
                l.run_id, l.stage, l.excluded_reason, l.utility, r.date AS run_date,
                t.score AS triage, t.kind AS triage_kind, d.score AS quality, d.fit AS fit,
                (SELECT re.label FROM rating_events re
                 WHERE re.article_id = a.id AND re.kind = 'explicit'
                 ORDER BY re.event_at DESC, re.id DESC LIMIT 1) AS rating,
                (SELECT ia.issue_date FROM issue_articles ia
                 WHERE ia.article_id = a.id ORDER BY ia.issue_date DESC LIMIT 1) AS published
         FROM articles a
         LEFT JOIN entries e ON e.id = a.best_entry_id
         LEFT JOIN candidate_runs l ON l.article_id = a.id
              AND l.run_id = (SELECT MAX(c2.run_id) FROM candidate_runs c2
                              WHERE c2.article_id = a.id)
         LEFT JOIN runs r ON r.id = l.run_id
         LEFT JOIN article_assessments t ON t.article_id = a.id AND t.stage = 'triage'
         LEFT JOIN article_assessments d ON d.article_id = a.id AND d.stage = 'deep'";

pub async fn list_articles(
    db: &Db,
    config: &Config,
    filters: &ArticleFilters,
    page: u32,
) -> Result<(Vec<ArticleListRow>, Pagination), sqlx::Error> {
    let (clauses, binds) = filters.where_clauses();
    let count_sql = format!("SELECT COUNT(*) FROM ({ARTICLE_INNER}) x WHERE 1 = 1{clauses}");
    let total: i64 = bind_all(dynamic_query(count_sql), &binds)
        .fetch_one(db.pool())
        .await?
        .get(0);
    let pagination = Pagination {
        page,
        per_page: ARTICLES_PER_PAGE,
        total,
    };
    let select_sql = format!(
        "SELECT * FROM ({ARTICLE_INNER}) x WHERE 1 = 1{clauses} ORDER BY {} LIMIT ? OFFSET ?",
        filters.order_by()
    );
    let rows = bind_all(dynamic_query(select_sql), &binds)
        .bind(i64::from(ARTICLES_PER_PAGE))
        .bind(pagination.offset())
        .fetch_all(db.pool())
        .await?;
    let rows = rows
        .iter()
        .map(|row| {
            let id: ArticleId = row.get("id");
            let rating: Option<String> = row.get("rating");
            ArticleListRow {
                id,
                href: format!("/dashboard/articles/{id}"),
                title: row.get("title"),
                feed_id: row.get("feed_id"),
                feed: row.get("feed_title"),
                first_seen: fmt_stored_time(Some(&row.get::<String, _>("first_seen")), config),
                words: row.get("word_count"),
                stage: row.get("stage"),
                reason: row.get("excluded_reason"),
                run_id: row.get("run_id"),
                run_date: row.get("run_date"),
                utility: fmt_opt(row.get("utility"), 1),
                triage: fmt_opt(row.get("triage"), 1),
                quality: fmt_opt(row.get("quality"), 1),
                fit: fmt_opt(row.get("fit"), 1),
                rating_class: widget_label(rating.as_deref()),
                rating,
                published: row.get("published"),
            }
        })
        .collect();
    Ok((rows, pagination))
}

#[derive(Template)]
#[template(path = "dashboard/articles.html")]
struct ArticlesTemplate {
    page: Page,
    articles: Vec<ArticleListRow>,
    filters: ArticleFilters,
    feed_value: String,
    stages: Vec<&'static str>,
    reasons: Vec<&'static str>,
    rated: Vec<&'static str>,
    kinds: Vec<&'static str>,
    sorts: Vec<&'static str>,
    pager: Pager,
}

async fn list(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<ArticlesQuery>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let filters = ArticleFilters::from_query(&query);
    let page_no = page_number(query.page);
    let (articles, pagination) = list_articles(&state.db, &config, &filters, page_no)
        .await
        .map_err(db_err)?;
    let pager = Pager::new(pagination, "/dashboard/articles", &filters.params());
    let mut kinds: Vec<&'static str> = TRIAGE_KINDS.to_vec();
    kinds.push(PROVIDER_REJECTED);
    let mut page = Page::new("Articles", viewer, "articles");
    page.flash = take_flash(&session).await?;
    Ok(Html(ArticlesTemplate {
        page,
        articles,
        feed_value: filters
            .feed
            .map(|feed| feed.to_string())
            .unwrap_or_default(),
        filters,
        stages: STAGES.to_vec(),
        reasons: REASONS.to_vec(),
        rated: RATED.to_vec(),
        kinds,
        sorts: ARTICLE_SORTS.iter().map(|(name, _)| *name).collect(),
        pager,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Article detail
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Facet {
    pub name: String,
    pub value: String,
}

/// One `article_assessments` row.
#[derive(Debug, Clone)]
pub struct AssessmentView {
    pub stage: String,
    pub rejected: bool,
    pub model: String,
    pub prompt_version: i64,
    pub profile_version: String,
    pub score: String,
    pub fit: String,
    pub kind: String,
    pub rationale: String,
    pub category: String,
    pub paywalled: bool,
    pub assessed_at: String,
    pub facets: Vec<Facet>,
}

/// One `candidate_runs` row of the article, newest first.
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub run_id: i64,
    pub run_href: String,
    pub date: String,
    pub status: String,
    pub stage: String,
    pub reason: Option<String>,
    pub admitted_first: Option<String>,
    pub admitted_rest: String,
    pub utility: String,
    pub rank: String,
    pub cluster: String,
    pub editor_why: Option<String>,
    pub signals: SignalsView,
}

#[derive(Debug, Clone)]
pub struct EmbeddingView {
    pub model: String,
    pub dimension: i64,
    pub created_at: String,
    pub input_hash: String,
}

#[derive(Debug, Clone)]
struct StoredEmbedding {
    view: EmbeddingView,
    vector: Vec<f32>,
}

#[derive(Debug, Clone)]
struct NearestArticleView {
    id: ArticleId,
    cosine: String,
    title: String,
    feed: String,
    first_seen: String,
    rating: Option<String>,
    rating_class: &'static str,
}

#[derive(Debug, Clone)]
pub struct RatingEventView {
    pub id: i64,
    pub event_at: String,
    pub issue_date: Option<String>,
    pub kind: String,
    pub source: String,
    pub label: String,
    pub label_class: &'static str,
    pub value: String,
    pub note: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone)]
struct InIssue {
    date: String,
    href: String,
    section: String,
    position: i64,
    is_lead: bool,
}

#[derive(Debug, Clone)]
struct SourceLine {
    kind: String,
    feed: String,
    category: Option<String>,
}

#[derive(Debug, Clone)]
struct SocialLine {
    source: String,
    score: i64,
    comments: i64,
    url: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/article.html")]
struct ArticleTemplate {
    page: Page,
    id: ArticleId,
    title: String,
    url: String,
    canonical_url: String,
    feed: String,
    feed_id: i64,
    category: Option<String>,
    author: Option<String>,
    published_at: String,
    first_seen: String,
    words: i64,
    excerpt_only: bool,
    image_count: i64,
    sources: Vec<SourceLine>,
    social: Vec<SocialLine>,
    in_issues: Vec<InIssue>,
    rating: Option<String>,
    rating_class: &'static str,
    widget: RatingWidget,
    explain: Option<String>,
    assessments: Vec<AssessmentView>,
    history: Vec<HistoryRow>,
    latest_signals: Option<SignalsView>,
    nearest_articles: Option<Vec<NearestArticleView>>,
    embedding: Option<EmbeddingView>,
    events: Vec<RatingEventView>,
}

pub async fn assessments(
    db: &Db,
    article_id: ArticleId,
    config: &Config,
) -> Result<Vec<AssessmentView>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT stage, model, prompt_version, profile_version, score, fit, kind, facets_json,
                rationale, category, paywalled_guess, assessed_at
         FROM article_assessments WHERE article_id = ? ORDER BY stage DESC",
    )
    .bind(article_id)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let kind: Option<String> = row.get("kind");
            let facets = row
                .get::<Option<String>, _>("facets_json")
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| value.as_object().cloned())
                .map(|object| {
                    object
                        .into_iter()
                        .map(|(name, value)| Facet {
                            name,
                            value: match value {
                                serde_json::Value::String(text) => text,
                                serde_json::Value::Array(items) => items
                                    .iter()
                                    .map(|item| {
                                        item.as_str()
                                            .map(str::to_string)
                                            .unwrap_or_else(|| item.to_string())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => other.to_string(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default();
            AssessmentView {
                stage: row.get("stage"),
                rejected: kind.as_deref() == Some(PROVIDER_REJECTED),
                model: row.get("model"),
                prompt_version: row.get("prompt_version"),
                profile_version: fmt_opt_int(row.get("profile_version")),
                score: fmt_opt(row.get("score"), 1),
                fit: fmt_opt(row.get("fit"), 1),
                kind: kind.unwrap_or_else(|| "—".into()),
                rationale: row
                    .get::<Option<String>, _>("rationale")
                    .unwrap_or_default(),
                category: row
                    .get::<Option<String>, _>("category")
                    .unwrap_or_else(|| "—".into()),
                paywalled: row.get::<i64, _>("paywalled_guess") != 0,
                assessed_at: fmt_stored_time(Some(&row.get::<String, _>("assessed_at")), config),
                facets,
            }
        })
        .collect())
}

pub async fn run_history(db: &Db, article_id: ArticleId) -> Result<Vec<HistoryRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT cr.run_id, r.date, r.status, cr.stage, cr.excluded_reason, cr.admitted_by,
                cr.signals_json, cr.utility, cr.rank_utility, cr.cluster_id, cr.cluster_rank,
                cr.editor_why
         FROM candidate_runs cr JOIN runs r ON r.id = cr.run_id
         WHERE cr.article_id = ? ORDER BY cr.run_id DESC",
    )
    .bind(article_id)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let run_id: i64 = row.get("run_id");
            let (admitted_first, rest) =
                admitted_by_parts(row.get::<Option<String>, _>("admitted_by").as_deref());
            HistoryRow {
                run_id,
                run_href: format!("/dashboard/runs/{run_id}"),
                date: row.get("date"),
                status: row.get("status"),
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
                editor_why: row.get("editor_why"),
                signals: SignalsView::from_json(&row.get::<String, _>("signals_json")),
            }
        })
        .collect())
}

pub async fn rating_events(
    db: &Db,
    article_id: ArticleId,
    config: &Config,
) -> Result<Vec<RatingEventView>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT re.id, re.issue_date, re.kind, re.source, re.label, re.value, re.note,
                re.event_at, u.username
         FROM rating_events re LEFT JOIN users u ON u.id = re.user_id
         WHERE re.article_id = ? ORDER BY re.event_at DESC, re.id DESC",
    )
    .bind(article_id)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let label: String = row.get("label");
            RatingEventView {
                id: row.get("id"),
                event_at: fmt_stored_time(Some(&row.get::<String, _>("event_at")), config),
                issue_date: row.get("issue_date"),
                kind: row.get("kind"),
                source: row.get("source"),
                label_class: widget_label(Some(&label)),
                label,
                value: format!("{:.2}", row.get::<f64, _>("value")),
                note: row.get("note"),
                user: row.get("username"),
            }
        })
        .collect())
}

async fn embedding(
    db: &Db,
    article_id: ArticleId,
    config: &Config,
) -> anyhow::Result<Option<StoredEmbedding>> {
    let row = sqlx::query(
        "SELECT model, dimension, created_at, input_hash, embedding FROM article_embeddings
         WHERE article_id = ?",
    )
    .bind(article_id)
    .fetch_optional(db.pool())
    .await?;
    let Some(row) = row else { return Ok(None) };
    let dimension: i64 = row.get("dimension");
    let decoded_dimension = usize::try_from(dimension)
        .map_err(|_| anyhow::anyhow!("invalid embedding dimension {dimension}"))?;
    let vector = decode_blob(&row.get::<Vec<u8>, _>("embedding"), decoded_dimension)?;
    Ok(Some(StoredEmbedding {
        view: EmbeddingView {
            model: row.get("model"),
            dimension,
            created_at: fmt_stored_time(Some(&row.get::<String, _>("created_at")), config),
            input_hash: row.get("input_hash"),
        },
        vector,
    }))
}

async fn nearest_article_views(
    db: &Db,
    article_id: ArticleId,
    stored: &StoredEmbedding,
    config: &Config,
) -> anyhow::Result<Vec<NearestArticleView>> {
    let dimension = usize::try_from(stored.view.dimension)
        .map_err(|_| anyhow::anyhow!("invalid embedding dimension {}", stored.view.dimension))?;
    let scored = embedding::nearest_articles(
        db,
        article_id,
        &stored.view.model,
        dimension,
        &stored.vector,
        NEAREST_ARTICLES,
    )
    .await?;
    let ids = scored.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let articles = db.get_articles(&ids).await?;
    let ratings = db
        .current_ratings(CURRENT_RATING_LOOKBACK_DAYS)
        .await?
        .into_iter()
        .map(|rating| (rating.article_id, rating.label))
        .collect::<HashMap<_, _>>();
    Ok(scored
        .into_iter()
        .filter_map(|(id, cosine)| {
            let article = articles.get(&id)?;
            let rating = ratings.get(&id).cloned();
            Some(NearestArticleView {
                id,
                cosine: format!("{cosine:.3}"),
                title: article.title.clone(),
                feed: article.feed_title.clone(),
                first_seen: crate::web::format_time(article.first_seen, config),
                rating_class: widget_label(rating.as_deref()),
                rating,
            })
        })
        .collect())
}

async fn detail(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(id): Path<ArticleId>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let db = &state.db;
    let Some(article) = db.get_article(id).await? else {
        return Err(WebError::NotFound);
    };

    let in_issues = sqlx::query(
        "SELECT issue_date, section, position, is_lead FROM issue_articles
         WHERE article_id = ? ORDER BY issue_date DESC",
    )
    .bind(id)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?
    .iter()
    .map(|row| {
        let date: String = row.get("issue_date");
        InIssue {
            href: format!("/issues/{date}/articles/{id}"),
            date,
            section: row.get("section"),
            position: row.get("position"),
            is_lead: row.get("is_lead"),
        }
    })
    .collect::<Vec<_>>();

    let events = rating_events(db, id, &config).await.map_err(db_err)?;
    let rating = events
        .iter()
        .find(|event| event.kind == "explicit")
        .map(|event| event.label.clone());
    let widget = RatingWidget {
        article_id: id,
        issue_date: in_issues
            .first()
            .map(|issue| issue.date.clone())
            .unwrap_or_default(),
        next: format!("/dashboard/articles/{id}"),
        current: widget_label(rating.as_deref()).to_string(),
        show_note: true,
    };

    let history = run_history(db, id).await.map_err(db_err)?;
    let explain = match history.first() {
        Some(latest) => match telemetry::explain_row(db, latest.run_id, id)
            .await
            .map_err(db_err)?
        {
            Some(row) => Some(telemetry::render_explain(db, &row).await.map_err(db_err)?),
            None => None,
        },
        None => None,
    };
    let latest_signals = history
        .first()
        .map(|latest| latest.signals.clone())
        .filter(|signals| !signals.empty);
    let assessments = assessments(db, id, &config).await.map_err(db_err)?;
    let embedding = embedding(db, id, &config)
        .await
        .map_err(WebError::Internal)?;
    let nearest_articles = match embedding.as_ref() {
        Some(stored) => Some(
            nearest_article_views(db, id, stored, &config)
                .await
                .map_err(WebError::Internal)?,
        ),
        None => None,
    };

    let mut page = Page::new(article.title.clone(), viewer, "articles");
    page.flash = take_flash(&session).await?;
    Ok(Html(ArticleTemplate {
        page,
        id,
        title: article.title.clone(),
        url: article.url.clone(),
        canonical_url: article.canonical_url.clone(),
        feed: article.feed_title.clone(),
        feed_id: article.feed_id,
        category: article.category.clone(),
        author: article.author.clone(),
        published_at: article
            .published_at
            .map(|at| crate::web::format_time(at, &config))
            .unwrap_or_else(|| "—".into()),
        first_seen: crate::web::format_time(article.first_seen, &config),
        words: article.word_count,
        excerpt_only: article.excerpt_only,
        image_count: article.image_count,
        sources: article
            .sources
            .iter()
            .map(|source| SourceLine {
                kind: format!("{:?}", source.kind),
                feed: source.feed_title.clone(),
                category: source.category.clone(),
            })
            .collect(),
        social: article
            .social
            .iter()
            .map(|social| SocialLine {
                source: format!("{:?}", social.source).to_lowercase(),
                score: social.score,
                comments: social.num_comments,
                url: social.item_url.clone(),
            })
            .collect(),
        in_issues,
        rating_class: widget_label(rating.as_deref()),
        rating,
        widget,
        explain,
        assessments,
        history,
        latest_signals,
        nearest_articles,
        embedding: embedding.map(|stored| stored.view),
        events,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curate::embedding::encode_blob;
    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, get, login_cookie, seed,
    };

    fn query(f: impl FnOnce(&mut ArticlesQuery)) -> ArticleFilters {
        let mut query = ArticlesQuery::default();
        f(&mut query);
        ArticleFilters::from_query(&query)
    }

    #[tokio::test]
    async fn articles_list_filters_and_sorts_are_allow_listed() {
        let seed = seed().await;
        let db = &seed.db;
        let config = Config::default();

        let all = query(|_| {});
        assert_eq!(all.sort, "first_seen");
        let (rows, pagination) = list_articles(db, &config, &all, 1).await.unwrap();
        assert_eq!(pagination.total, 8);
        assert_eq!(rows.len(), 8);
        let one = rows.iter().find(|row| row.id == 1).unwrap();
        assert_eq!(one.stage.as_deref(), Some("selected"), "latest run's row");
        assert_eq!(one.run_id, Some(seed.run_id));
        assert_eq!(one.rating.as_deref(), Some("loved"));
        assert_eq!(one.published.as_deref(), Some("2026-09-02"));
        assert_eq!(one.quality, "8.8");

        let bogus = query(|q| {
            q.sort = Some("id; DROP TABLE articles".into());
            q.stage = Some("nope".into());
            q.rated = Some("' OR 1=1".into());
            q.from = Some("not a date".into());
            q.feed = Some("abc".into());
        });
        assert_eq!(bogus.sort, "first_seen");
        assert_eq!(bogus.stage, None);
        assert_eq!(bogus.rated, None);
        assert_eq!(bogus.from, None);
        assert_eq!(bogus.feed, None);
        let (rows, _) = list_articles(db, &config, &bogus, 1).await.unwrap();
        assert_eq!(rows.len(), 8, "unknown values fall back, never error");

        let by_feed = query(|q| q.feed = Some("20".into()));
        let (rows, _) = list_articles(db, &config, &by_feed, 1).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.feed == "Beta Weekly"));

        let selected = query(|q| q.stage = Some("selected".into()));
        let (rows, _) = list_articles(db, &config, &selected, 1).await.unwrap();
        assert_eq!(rows.len(), 2);

        let reason = query(|q| q.reason = Some("blocked".into()));
        let (rows, _) = list_articles(db, &config, &reason, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 7);

        let loved = query(|q| q.rated = Some("loved".into()));
        let (rows, _) = list_articles(db, &config, &loved, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 1);
        let good = query(|q| q.rated = Some("good".into()));
        let (rows, _) = list_articles(db, &config, &good, 1).await.unwrap();
        assert!(rows.is_empty(), "superseded events do not count");
        let unrated = query(|q| q.rated = Some("none".into()));
        let (rows, _) = list_articles(db, &config, &unrated, 1).await.unwrap();
        assert_eq!(rows.len(), 7);

        let published = query(|q| q.published = Some("yes".into()));
        let (rows, _) = list_articles(db, &config, &published, 1).await.unwrap();
        assert_eq!(rows.len(), 2);

        let window = query(|q| {
            q.from = Some("2026-09-02".into());
            q.to = Some("2026-09-02".into());
        });
        let (rows, _) = list_articles(db, &config, &window, 1).await.unwrap();
        assert_eq!(rows.len(), 3, "ids 1, 4, 7 were first seen on the 2nd");

        let kind = query(|q| q.kind = Some("provider_rejected".into()));
        let (rows, _) = list_articles(db, &config, &kind, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 3);

        let search = query(|q| q.q = Some("example.com/8".into()));
        let (rows, _) = list_articles(db, &config, &search, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 8);

        let by_words = query(|q| q.sort = Some("words".into()));
        let (rows, _) = list_articles(db, &config, &by_words, 1).await.unwrap();
        assert_eq!(rows[0].id, 8);
        let by_utility = query(|q| q.sort = Some("utility".into()));
        let (rows, _) = list_articles(db, &config, &by_utility, 1).await.unwrap();
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[7].utility, "—");
        let by_title = query(|q| q.sort = Some("title".into()));
        let (rows, _) = list_articles(db, &config, &by_title, 1).await.unwrap();
        assert_eq!(rows[0].id, 1);
    }

    #[tokio::test]
    async fn article_detail_shows_assessments_run_history_and_rating_events() {
        let seed = seed().await;
        let config = Config::default();
        for (article_id, vector) in [
            (1, [1.0_f32, 0.0_f32]),
            (2, [0.8_f32, 0.6_f32]),
            (3, [0.6_f32, 0.8_f32]),
        ] {
            sqlx::query(
                "INSERT INTO article_embeddings
                     (article_id, model, dimension, input_hash, embedding, created_at)
                 VALUES (?, 'voyage-4-lite', 2, ?, ?, '2026-09-02T05:30:30Z')
                 ON CONFLICT(article_id) DO UPDATE SET
                     model = excluded.model, dimension = excluded.dimension,
                     input_hash = excluded.input_hash, embedding = excluded.embedding,
                     created_at = excluded.created_at",
            )
            .bind(article_id)
            .bind(if article_id == 1 {
                "abc123"
            } else {
                "nearest-hash"
            })
            .bind(encode_blob(&vector).unwrap())
            .execute(seed.db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO rating_events
                 (article_id, kind, source, label, value, event_at)
             VALUES (2, 'explicit', 'cli', 'good', 0.35, '2026-09-02T11:00:00Z')",
        )
        .execute(seed.db.pool())
        .await
        .unwrap();

        let views = assessments(&seed.db, 1, &config).await.unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].stage, "triage");
        assert_eq!(views[0].score, "7.0");
        assert_eq!(views[1].stage, "deep");
        assert_eq!(views[1].fit, "6.0");
        assert_eq!(views[1].category, "Top Stories");
        assert_eq!(views[1].facets[0].name, "depth");
        let rejected = assessments(&seed.db, 3, &config).await.unwrap();
        assert!(rejected[0].rejected);

        let history = run_history(&seed.db, 1).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].run_id, seed.run_id);
        assert_eq!(history[0].stage, "selected");
        assert_eq!(history[0].admitted_first.as_deref(), Some("triage"));
        assert_eq!(history[1].run_id, seed.earlier_run_id);
        assert!(history[1].signals.empty);

        let events = rating_events(&seed.db, 1, &config).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].label, "loved");
        assert_eq!(events[1].label, "good");
        assert_eq!(events[1].note.as_deref(), Some("note good"));

        let app = app_with_users(&seed.db).await;
        let list = assert_admin_only(&app, "/dashboard/articles").await;
        assert!(list.contains("Article 1 about prose"), "{list}");
        assert!(list.contains("/dashboard/articles/1"), "{list}");
        assert!(
            list.contains("<option value=\"repo\">repo</option>"),
            "{list}"
        );
        assert!(
            list.contains("<option value=\"fiction\">fiction</option>"),
            "{list}"
        );

        let body = assert_admin_only(&app, "/dashboard/articles/1").await;
        assert!(body.contains("Careful and first-hand"), "{body}");
        assert!(body.contains("A specific argument"), "{body}");
        assert!(body.contains("topic_group"), "{body}");
        assert!(
            body.contains(&format!("/dashboard/runs/{}", seed.earlier_run_id)),
            "{body}"
        );
        assert!(body.contains("note good"), "{body}");
        assert!(body.contains("note loved"), "{body}");
        assert!(body.contains("voyage-4-lite"), "{body}");
        assert!(body.contains("abc123"), "{body}");
        assert!(body.contains("Gaussian Splatting"), "{body}");
        assert!(
            body.contains("article 1: Article 1 about prose"),
            "explain: {body}"
        );
        assert!(body.contains("name=\"note\""), "note field: {body}");
        assert!(
            body.contains("value=\"loved\" data-label=\"loved\" class=\"active\""),
            "{body}"
        );
        assert!(body.contains("Top Stories"), "{body}");
        assert!(body.contains("Alpha Blog"), "{body}");
        let nearest = body
            .split_once("<h2>Nearest articles (any)</h2>")
            .expect("nearest heading")
            .1
            .split_once("<h2>Embedding</h2>")
            .expect("embedding heading")
            .0;
        let second = nearest
            .find("href=\"/dashboard/articles/2\">Article 2 about graphs")
            .expect("nearest article 2");
        let third = nearest
            .find("href=\"/dashboard/articles/3\">Article 3 about prose")
            .expect("nearest article 3");
        assert!(second < third, "higher cosine must render first: {nearest}");
        assert!(nearest.contains(">0.800</td>"), "{nearest}");
        assert!(nearest.contains(">0.600</td>"), "{nearest}");
        assert!(nearest.contains("badge good\">good"), "{nearest}");
        assert!(nearest.contains(">unrated</span>"), "{nearest}");
        assert!(
            !nearest.contains("href=\"/dashboard/articles/1\""),
            "the article itself must be excluded: {nearest}"
        );

        let rejected = assert_admin_only(&app, "/dashboard/articles/3").await;
        assert!(rejected.contains("rejected by provider"), "{rejected}");
        assert!(rejected.contains("Content Exists Risk"), "{rejected}");

        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let missing = get(&app, "/dashboard/articles/999", Some(&admin)).await;
        assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
        let filtered = get(
            &app,
            "/dashboard/articles?rated=loved&sort=nope&feed=x&page=0",
            Some(&admin),
        )
        .await;
        assert_eq!(filtered.status(), axum::http::StatusCode::OK);
        let filtered = crate::web::dashboard::tests::response_text(filtered).await;
        assert!(filtered.contains("Article 1 about prose"), "{filtered}");
        assert!(!filtered.contains("Article 2 about graphs"), "{filtered}");
    }
}
