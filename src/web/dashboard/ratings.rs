//! Dashboard: the ratings page (`/dashboard/ratings`, web plan §10).
//!
//! The **Current** tab shows one row per rated article with every way the
//! verdict enters the ranker: its decayed neighbour weight (curation plan
//! §9.2), the feed credit (§9.3), whether it sits in the prompt's verdict block
//! (§8.4) and in the weekly rebuild set (§8.3), and how many candidates of the
//! last run listed it among their nearest rated neighbours. The **Events** tab
//! is the append-only `rating_events` history.

use std::collections::{BTreeMap, HashMap, HashSet};

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use serde::Deserialize;
use sqlx::Row;

use crate::config::Config;
use crate::curate::profile::{MAX_RATINGS_IN_REBUILD, REBUILD_INTERVAL_DAYS};
use crate::curate::signals::{self, PreferenceState};
use crate::curate::telemetry::SignalsJson;
use crate::db::{Db, DbError};
use crate::server::AppState;
use crate::types::{Article, ArticleId, FeedId, RatedArticle};
use crate::web::rate::RatingWidget;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, Pagination, WebError, encode_component, format_time, take_flash};

/// The verdict block and the rebuild set are bounded by count, not age, so the
/// page lists every current verdict (curation plan §8.3, §8.4).
const CURRENT_LOOKBACK_DAYS: i64 = 36_500;
const EVENTS_PER_PAGE: u32 = 100;

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new().route("/dashboard/ratings", get(index))
}

// ---------------------------------------------------------------------------
// Contributions (pure; §10 table columns)
// ---------------------------------------------------------------------------

/// One direct feed's share of a rating (curation plan §9.3).
#[derive(Debug, Clone, PartialEq)]
pub struct FeedCredit {
    pub feed_id: FeedId,
    pub feed_title: String,
    /// `value × decay / n` over the article's `n` direct feeds.
    pub credit: f64,
}

/// How one current verdict enters the algorithm (§10).
#[derive(Debug, Clone, PartialEq)]
pub struct Contribution {
    pub age_days: f64,
    /// `0.5 ^ (age / half_life)` (`signals::decay`).
    pub decay: f64,
    /// `value × decay`, the `w_i` of §9.2; `None` without an embedding.
    pub neighbour_weight: Option<f64>,
    /// Older than `rating_lookback_days`: the preference state skips it.
    pub beyond_lookback: bool,
    pub feed_credits: Vec<FeedCredit>,
    /// Rank among current non-cleared verdicts, newest first; `None` for `cleared`.
    pub rank: Option<usize>,
    pub in_prompt: bool,
    pub in_rebuild: bool,
}

/// Compute the §10 columns for one verdict. `rank` is the article's position
/// among the current non-cleared verdicts ordered newest first.
pub fn contribution(
    rating: &RatedArticle,
    article: Option<&Article>,
    has_embedding: bool,
    rank: Option<usize>,
    now: Timestamp,
    config: &Config,
) -> Contribution {
    let ranking = &config.curation.ranking;
    let age_days = (now.as_second() - rating.event_at.as_second()).max(0) as f64 / 86_400.0;
    let decay = signals::decay(age_days, ranking.rating_half_life_days);
    let weight = rating.value * decay;
    let feeds = article.map(signals::direct_feeds).unwrap_or_default();
    let feed_credits = if rating.label == "cleared" || feeds.is_empty() {
        Vec::new()
    } else {
        let credit = weight / feeds.len() as f64;
        feeds
            .iter()
            .map(|feed_id| FeedCredit {
                feed_id: *feed_id,
                feed_title: feed_title_for(article, *feed_id),
                credit,
            })
            .collect()
    };
    let active = rating.label != "cleared";
    Contribution {
        age_days,
        decay,
        neighbour_weight: (active && has_embedding).then_some(weight),
        beyond_lookback: age_days > ranking.rating_lookback_days as f64,
        feed_credits,
        rank,
        in_prompt: rank.is_some_and(|rank| rank < config.curation.feedback.verdicts_in_prompt),
        in_rebuild: rank.is_some_and(|rank| rank < MAX_RATINGS_IN_REBUILD),
    }
}

fn feed_title_for(article: Option<&Article>, feed_id: FeedId) -> String {
    let Some(article) = article else {
        return format!("feed {feed_id}");
    };
    article
        .sources
        .iter()
        .find(|source| source.feed_id == feed_id)
        .map(|source| source.feed_title.clone())
        .or_else(|| (article.feed_id == feed_id).then(|| article.feed_title.clone()))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("feed {feed_id}"))
}

/// How often a rated article appeared among candidates' nearest neighbours in
/// one run (§10 "Used last run").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NeighbourUse {
    pub total: usize,
    pub selected: usize,
}

/// The latest run that was not a dry run.
#[derive(Debug, Clone)]
pub struct LastRun {
    pub id: i64,
    pub date: String,
    pub status: String,
}

pub async fn last_real_run(db: &Db) -> Result<Option<LastRun>, DbError> {
    let row = sqlx::query(
        "SELECT id, date, status FROM runs WHERE status != 'dry_run' ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| LastRun {
        id: row.get("id"),
        date: row.get("date"),
        status: row.get("status"),
    }))
}

/// Count, per rated article, the candidates of `run_id` whose
/// `signals_json.neighbours` list it, and how many of those were selected.
pub async fn neighbour_usage(
    db: &Db,
    run_id: i64,
) -> Result<HashMap<ArticleId, NeighbourUse>, DbError> {
    let rows = sqlx::query(
        "SELECT stage, signals_json FROM candidate_runs
         WHERE run_id = ? AND signals_json LIKE '%\"neighbours\":[{%'",
    )
    .bind(run_id)
    .fetch_all(db.pool())
    .await?;
    let mut usage: HashMap<ArticleId, NeighbourUse> = HashMap::new();
    for row in rows {
        let stage: String = row.get("stage");
        let raw: String = row.get("signals_json");
        let Ok(signals) = serde_json::from_str::<SignalsJson>(&raw) else {
            continue;
        };
        let mut seen = HashSet::new();
        for neighbour in signals.neighbours {
            if !seen.insert(neighbour.article_id) {
                continue;
            }
            let entry = usage.entry(neighbour.article_id).or_default();
            entry.total += 1;
            if stage == "selected" {
                entry.selected += 1;
            }
        }
    }
    Ok(usage)
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RatingsQuery {
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    feed: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    page: Option<String>,
}

fn clean(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Widget labels (`loved|good|down|cleared`) → the stored event label.
fn event_label(widget: &str) -> Option<&'static str> {
    match widget {
        "loved" => Some("loved"),
        "good" => Some("good"),
        "down" => Some("not_for_me"),
        "cleared" => Some("cleared"),
        _ => None,
    }
}

/// Stored event labels → the widget/badge label and its display text.
fn widget_label(label: &str) -> (&'static str, &'static str) {
    match label {
        "loved" => ("loved", "Loved it"),
        "good" => ("good", "Good"),
        "not_for_me" | "down" => ("down", "Not for me"),
        "cleared" => ("cleared", "Cleared"),
        _ => ("", "Unknown"),
    }
}

fn valid_date(value: Option<String>) -> Option<String> {
    value.filter(|value| value.parse::<jiff::civil::Date>().is_ok())
}

fn source_is_valid(source: &str) -> bool {
    !source.is_empty()
        && source.len() <= 32
        && source
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Debug, Clone, Default)]
struct Filters {
    tab: String,
    label: Option<String>,
    source: Option<String>,
    feed: Option<FeedId>,
    q: Option<String>,
    user: Option<String>,
    from: Option<String>,
    to: Option<String>,
    page: u32,
}

impl Filters {
    fn parse(query: RatingsQuery) -> Self {
        let tab = match query.tab.as_deref() {
            Some("events") => "events",
            _ => "current",
        }
        .to_string();
        Self {
            tab,
            label: clean(&query.label).filter(|label| event_label(label).is_some()),
            source: clean(&query.source).filter(|source| source_is_valid(source)),
            feed: clean(&query.feed).and_then(|feed| feed.parse::<FeedId>().ok()),
            q: clean(&query.q).map(|q| q.chars().take(200).collect()),
            user: clean(&query.user).filter(|user| user.len() <= 32),
            from: valid_date(clean(&query.from)),
            to: valid_date(clean(&query.to)),
            page: clean(&query.page)
                .and_then(|page| page.parse::<u32>().ok())
                .unwrap_or(1)
                .max(1),
        }
    }

    /// The page's own URL with every active filter, used as the widget's `next`.
    fn href(&self, page: Option<u32>) -> String {
        let mut params = vec![("tab", self.tab.clone())];
        if let Some(label) = &self.label {
            params.push(("label", label.clone()));
        }
        if let Some(source) = &self.source {
            params.push(("source", source.clone()));
        }
        if let Some(feed) = self.feed {
            params.push(("feed", feed.to_string()));
        }
        if let Some(q) = &self.q {
            params.push(("q", q.clone()));
        }
        if let Some(user) = &self.user {
            params.push(("user", user.clone()));
        }
        if let Some(from) = &self.from {
            params.push(("from", from.clone()));
        }
        if let Some(to) = &self.to {
            params.push(("to", to.clone()));
        }
        if let Some(page) = page.filter(|page| *page > 1) {
            params.push(("page", page.to_string()));
        }
        let query = params
            .iter()
            .map(|(key, value)| format!("{key}={}", encode_component(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("/dashboard/ratings?{query}")
    }
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

struct FeedCreditView {
    title: String,
    credit: String,
}

struct CurrentRow {
    article_id: ArticleId,
    title: String,
    feed_title: String,
    issue_date: String,
    badge: String,
    widget: RatingWidget,
    when: String,
    source: String,
    username: String,
    note: String,
    age_days: String,
    decay: String,
    has_embedding: bool,
    neighbour_weight: String,
    beyond_lookback: bool,
    feed_credits: Vec<FeedCreditView>,
    in_prompt: bool,
    in_rebuild: bool,
    used_total: usize,
    used_selected: usize,
}

struct EventRow {
    id: i64,
    article_id: ArticleId,
    title: String,
    issue_date: String,
    kind: String,
    badge: String,
    verdict: String,
    value: String,
    when: String,
    source: String,
    username: String,
    note: String,
    superseded: bool,
}

struct FeedOption {
    id: FeedId,
    title: String,
}

/// Configuration values inlined into the "How ratings enter the algorithm" block.
struct HowValues {
    loved: String,
    good: String,
    not_for_me: String,
    verdicts_in_prompt: usize,
    rebuild_interval_days: i64,
    max_ratings_in_rebuild: usize,
    half_life_days: String,
    lookback_days: i64,
    neighbour_k: usize,
    negative_coefficient: String,
    knn_floor: usize,
    knn_full: usize,
    knn_weight: String,
    feed_floor: usize,
    feed_full: usize,
    feed_weight: String,
}

impl HowValues {
    fn from_config(config: &Config) -> Self {
        let feedback = &config.curation.feedback;
        let ranking = &config.curation.ranking;
        Self {
            loved: format!("{:+.2}", feedback.loved_value),
            good: format!("{:+.2}", feedback.good_value),
            not_for_me: format!("{:+.2}", feedback.not_for_me_value),
            verdicts_in_prompt: feedback.verdicts_in_prompt,
            rebuild_interval_days: REBUILD_INTERVAL_DAYS,
            max_ratings_in_rebuild: MAX_RATINGS_IN_REBUILD,
            half_life_days: format!("{}", ranking.rating_half_life_days),
            lookback_days: ranking.rating_lookback_days,
            neighbour_k: ranking.neighbour_k,
            negative_coefficient: format!("{}", ranking.negative_coefficient),
            knn_floor: ranking.knn_floor,
            knn_full: ranking.knn_full,
            knn_weight: format!("{}", ranking.weights.preliminary.knn),
            feed_floor: ranking.feed_floor,
            feed_full: ranking.feed_full,
            feed_weight: format!("{}", ranking.weights.preliminary.feed),
        }
    }
}

#[derive(Template)]
#[template(path = "dashboard/ratings.html")]
struct RatingsTemplate {
    page: Page,
    tab: String,
    summary_line: String,
    no_embedding_count: usize,
    how: HowValues,
    filter_label: String,
    filter_source: String,
    filter_feed: String,
    filter_q: String,
    filter_user: String,
    filter_from: String,
    filter_to: String,
    sources: Vec<String>,
    feeds: Vec<FeedOption>,
    usernames: Vec<String>,
    current_href: String,
    events_href: String,
    current: Vec<CurrentRow>,
    current_total: usize,
    last_run: Option<LastRun>,
    events: Vec<EventRow>,
    pagination: Pagination,
    prev_href: String,
    next_href: String,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<RatingsQuery>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: "/dashboard/ratings".into(),
        })?;
    let config = state.config();
    let filters = Filters::parse(query);
    let now = Timestamp::now();
    let db = &state.db;

    let preference = PreferenceState::load(db, &config.voyage, &config.curation.ranking, now)
        .await
        .map_err(WebError::Internal)?
        .summary();
    let ratings = db
        .current_ratings_including_cleared(CURRENT_LOOKBACK_DAYS)
        .await?;
    let active_count = ratings
        .iter()
        .filter(|rating| rating.label != "cleared")
        .count();
    let embedded = embedded_article_ids(db, &config).await?;
    let no_embedding_count = ratings
        .iter()
        .filter(|rating| rating.label != "cleared" && !embedded.contains(&rating.article_id))
        .count();
    let summary_line = format!(
        "{} rated articles with embeddings → neighbour signal at {:.0}% (floor {}, full {}) · {} attributable feed ratings → feed affinity at {:.0}% · {} verdicts in the prompt · {} in the weekly rebuild set",
        preference.rated_with_embeddings,
        preference.knn_gate * 100.0,
        config.curation.ranking.knn_floor,
        config.curation.ranking.knn_full,
        preference.attributable_feed_ratings,
        preference.feed_gate * 100.0,
        active_count.min(config.curation.feedback.verdicts_in_prompt),
        active_count.min(MAX_RATINGS_IN_REBUILD),
    );

    let usernames_by_id = usernames(db).await?;
    let sources = event_sources(db).await?;
    let last_run = last_real_run(db).await?;

    let mut page = Page::new("Ratings", Some(viewer), "ratings");
    page.flash = take_flash(&session).await?;
    let mut template = RatingsTemplate {
        page,
        tab: filters.tab.clone(),
        summary_line,
        no_embedding_count,
        how: HowValues::from_config(&config),
        filter_label: filters.label.clone().unwrap_or_default(),
        filter_source: filters.source.clone().unwrap_or_default(),
        filter_feed: filters
            .feed
            .map(|feed| feed.to_string())
            .unwrap_or_default(),
        filter_q: filters.q.clone().unwrap_or_default(),
        filter_user: filters.user.clone().unwrap_or_default(),
        filter_from: filters.from.clone().unwrap_or_default(),
        filter_to: filters.to.clone().unwrap_or_default(),
        sources,
        feeds: Vec::new(),
        usernames: usernames_by_id.values().cloned().collect(),
        current_href: Filters {
            tab: "current".into(),
            ..filters.clone()
        }
        .href(None),
        events_href: Filters {
            tab: "events".into(),
            ..filters.clone()
        }
        .href(None),
        current: Vec::new(),
        current_total: ratings.len(),
        last_run,
        events: Vec::new(),
        pagination: Pagination {
            page: 1,
            per_page: EVENTS_PER_PAGE,
            total: 0,
        },
        prev_href: String::new(),
        next_href: String::new(),
    };

    if filters.tab == "events" {
        let (events, total) = load_events(db, &config, &filters, &usernames_by_id).await?;
        template.pagination = Pagination {
            page: filters.page,
            per_page: EVENTS_PER_PAGE,
            total,
        };
        if filters.page > 1 {
            template.prev_href = filters.href(Some(filters.page - 1));
        }
        if filters.page < template.pagination.pages() {
            template.next_href = filters.href(Some(filters.page + 1));
        }
        template.events = events;
    } else {
        let usage = match &template.last_run {
            Some(run) => neighbour_usage(db, run.id).await?,
            None => HashMap::new(),
        };
        let (rows, feeds) = build_current_rows(
            db,
            &config,
            &filters,
            &ratings,
            &embedded,
            &usernames_by_id,
            &usage,
            now,
        )
        .await?;
        template.current = rows;
        template.feeds = feeds;
    }
    Ok(Html(template).into_response())
}

async fn embedded_article_ids(db: &Db, config: &Config) -> Result<HashSet<ArticleId>, WebError> {
    let rows =
        sqlx::query("SELECT article_id FROM article_embeddings WHERE model = ? AND dimension = ?")
            .bind(&config.voyage.model)
            .bind(config.voyage.output_dimension as i64)
            .fetch_all(db.pool())
            .await
            .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<ArticleId, _>("article_id"))
        .collect())
}

async fn usernames(db: &Db) -> Result<BTreeMap<i64, String>, WebError> {
    let rows = sqlx::query("SELECT id, username FROM users ORDER BY username")
        .fetch_all(db.pool())
        .await
        .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get::<i64, _>("id"), row.get::<String, _>("username")))
        .collect())
}

async fn event_sources(db: &Db) -> Result<Vec<String>, WebError> {
    let rows = sqlx::query("SELECT DISTINCT source FROM rating_events ORDER BY source")
        .fetch_all(db.pool())
        .await
        .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("source"))
        .collect())
}

fn username_for(usernames: &BTreeMap<i64, String>, user_id: Option<i64>, source: &str) -> String {
    match user_id.and_then(|id| usernames.get(&id)) {
        Some(username) => username.clone(),
        None if source == "epub" => "e-reader link".into(),
        None => "—".into(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_current_rows(
    db: &Db,
    config: &Config,
    filters: &Filters,
    ratings: &[RatedArticle],
    embedded: &HashSet<ArticleId>,
    usernames: &BTreeMap<i64, String>,
    usage: &HashMap<ArticleId, NeighbourUse>,
    now: Timestamp,
) -> Result<(Vec<CurrentRow>, Vec<FeedOption>), WebError> {
    let next = filters.href(None);
    let mut rows = Vec::new();
    let mut feeds: BTreeMap<FeedId, String> = BTreeMap::new();
    let mut rank = 0usize;
    for rating in ratings {
        let article = db.get_article(rating.article_id).await?;
        let this_rank = if rating.label == "cleared" {
            None
        } else {
            let current = rank;
            rank += 1;
            Some(current)
        };
        let has_embedding = embedded.contains(&rating.article_id);
        let contribution = contribution(
            rating,
            article.as_ref(),
            has_embedding,
            this_rank,
            now,
            config,
        );
        let direct = article
            .as_ref()
            .map(signals::direct_feeds)
            .unwrap_or_default();
        for feed_id in &direct {
            feeds
                .entry(*feed_id)
                .or_insert_with(|| feed_title_for(article.as_ref(), *feed_id));
        }

        let (badge, _) = widget_label(&rating.label);
        if filters.label.as_deref().is_some_and(|label| label != badge) {
            continue;
        }
        if let Some(feed) = filters.feed
            && !direct.contains(&feed)
        {
            continue;
        }
        if let Some(q) = &filters.q
            && !rating.title.to_lowercase().contains(&q.to_lowercase())
        {
            continue;
        }
        let source = event_source_for(db, rating).await?;
        if filters
            .source
            .as_deref()
            .is_some_and(|wanted| wanted != source)
        {
            continue;
        }
        let used = usage.get(&rating.article_id).copied().unwrap_or_default();
        let issue_date = rating
            .issue_date
            .map(|date| date.to_string())
            .unwrap_or_default();
        rows.push(CurrentRow {
            article_id: rating.article_id,
            title: if rating.title.trim().is_empty() {
                format!("article {}", rating.article_id)
            } else {
                rating.title.clone()
            },
            feed_title: rating.feed_title.clone(),
            issue_date: issue_date.clone(),
            badge: badge.to_string(),
            widget: RatingWidget {
                article_id: rating.article_id,
                issue_date,
                next: next.clone(),
                current: badge.to_string(),
                show_note: true,
            },
            when: format_time(rating.event_at, config),
            username: username_for(usernames, rating.user_id, &source),
            source,
            note: rating.note.clone().unwrap_or_default(),
            age_days: format!("{:.0}", contribution.age_days),
            decay: format!("{:.3}", contribution.decay),
            has_embedding,
            neighbour_weight: contribution
                .neighbour_weight
                .map(|weight| format!("{weight:+.3}"))
                .unwrap_or_default(),
            beyond_lookback: contribution.beyond_lookback,
            feed_credits: contribution
                .feed_credits
                .iter()
                .map(|credit| FeedCreditView {
                    title: credit.feed_title.clone(),
                    credit: format!("{:+.3}", credit.credit),
                })
                .collect(),
            in_prompt: contribution.in_prompt,
            in_rebuild: contribution.in_rebuild,
            used_total: used.total,
            used_selected: used.selected,
        });
    }
    let feeds = feeds
        .into_iter()
        .map(|(id, title)| FeedOption { id, title })
        .collect();
    Ok((rows, feeds))
}

/// The `source` of the event behind a current verdict (`RatedArticle` does not
/// carry it).
async fn event_source_for(db: &Db, rating: &RatedArticle) -> Result<String, WebError> {
    let row = sqlx::query(
        "SELECT source FROM rating_events
         WHERE article_id = ? AND kind = 'explicit'
         ORDER BY event_at DESC, id DESC LIMIT 1",
    )
    .bind(rating.article_id)
    .fetch_optional(db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(row
        .map(|row| row.get::<String, _>("source"))
        .unwrap_or_default())
}

async fn load_events(
    db: &Db,
    config: &Config,
    filters: &Filters,
    usernames: &BTreeMap<i64, String>,
) -> Result<(Vec<EventRow>, i64), WebError> {
    // Every clause below is fixed text chosen from the allow-listed filters;
    // only values are bound, which is what `AssertSqlSafe` asserts.
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(label) = &filters.label
        && let Some(stored) = event_label(label)
    {
        clauses.push("re.label = ?");
        binds.push(stored.to_string());
    }
    if let Some(source) = &filters.source {
        clauses.push("re.source = ?");
        binds.push(source.clone());
    }
    if let Some(user) = &filters.user {
        clauses.push("u.username = ? COLLATE NOCASE");
        binds.push(user.clone());
    }
    if let Some(from) = &filters.from {
        clauses.push("substr(re.event_at, 1, 10) >= ?");
        binds.push(from.clone());
    }
    if let Some(to) = &filters.to {
        clauses.push("substr(re.event_at, 1, 10) <= ?");
        binds.push(to.clone());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let base = format!(
        "FROM rating_events re
         LEFT JOIN articles a ON a.id = re.article_id
         LEFT JOIN users u ON u.id = re.user_id
         {where_sql}"
    );

    let count_sql = format!("SELECT COUNT(*) AS n {base}");
    let mut count = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    for value in &binds {
        count = count.bind(value);
    }
    let total: i64 = count
        .fetch_one(db.pool())
        .await
        .map_err(DbError::from)?
        .get("n");

    let pagination = Pagination {
        page: filters.page,
        per_page: EVENTS_PER_PAGE,
        total,
    };
    let sql = format!(
        "SELECT re.id, re.article_id, re.issue_date, re.kind, re.source, re.label, re.value,
                re.note, re.event_at, re.user_id, COALESCE(a.title, '') AS title,
                EXISTS (
                    SELECT 1 FROM rating_events later
                    WHERE later.article_id = re.article_id AND later.kind = 'explicit'
                      AND (later.event_at > re.event_at
                           OR (later.event_at = re.event_at AND later.id > re.id))
                ) AS superseded
         {base}
         ORDER BY re.event_at DESC, re.id DESC
         LIMIT ? OFFSET ?"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for value in &binds {
        query = query.bind(value);
    }
    let rows = query
        .bind(i64::from(EVENTS_PER_PAGE))
        .bind(pagination.offset())
        .fetch_all(db.pool())
        .await
        .map_err(DbError::from)?;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let label: String = row.get("label");
        let (badge, verdict) = widget_label(&label);
        let source: String = row.get("source");
        let event_at =
            crate::db::parse_ts("rating_events.event_at", &row.get::<String, _>("event_at"))?;
        let article_id: ArticleId = row.get("article_id");
        let title: String = row.get("title");
        events.push(EventRow {
            id: row.get("id"),
            article_id,
            title: if title.trim().is_empty() {
                format!("article {article_id}")
            } else {
                title
            },
            issue_date: row
                .get::<Option<String>, _>("issue_date")
                .unwrap_or_default(),
            kind: row.get("kind"),
            badge: badge.to_string(),
            verdict: if badge.is_empty() {
                label
            } else {
                verdict.to_string()
            },
            value: format!("{:+.2}", row.get::<f64, _>("value")),
            when: format_time(event_at, config),
            username: username_for(usernames, row.get("user_id"), &source),
            source,
            note: row.get::<Option<String>, _>("note").unwrap_or_default(),
            superseded: row.get::<bool, _>("superseded"),
        });
    }
    Ok((events, total))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::types::{Entry, RatingEvent, SourceKind, SourceRef};
    use crate::web::users;

    fn rated(article_id: ArticleId, label: &str, value: f64, event_at: &str) -> RatedArticle {
        RatedArticle {
            article_id,
            user_id: None,
            issue_date: None,
            title: format!("Article {article_id}"),
            feed_title: "Example Feed".into(),
            summary: None,
            facets: None,
            note: None,
            value,
            label: label.into(),
            event_at: event_at.parse().unwrap(),
        }
    }

    fn two_feed_article() -> Article {
        let mut article = crate::epub::fixtures::article(1, 1001, "Two feeds");
        article.sources.push(SourceRef {
            entry_id: 1002,
            feed_id: 9,
            feed_title: "Second Feed".into(),
            category: None,
            kind: SourceKind::Feed,
        });
        article.sources.push(SourceRef {
            entry_id: 1003,
            feed_id: 11,
            feed_title: "Scour".into(),
            category: None,
            kind: SourceKind::Scour,
        });
        article
    }

    #[test]
    fn contribution_matches_hand_checked_values() {
        let config = Config::default();
        let now: Timestamp = "2026-09-03T00:00:00Z".parse().unwrap();
        // Loved 60 days ago with the 60-day half-life: decay 0.5, weight 0.5,
        // split over the two direct feeds (the Scour source is not direct).
        let rating = rated(1, "loved", 1.0, "2026-07-05T00:00:00Z");
        let article = two_feed_article();
        let c = contribution(&rating, Some(&article), true, Some(59), now, &config);
        assert!((c.age_days - 60.0).abs() < 1e-9);
        assert!((c.decay - 0.5).abs() < 1e-9);
        assert_eq!(c.neighbour_weight, Some(0.5));
        assert!(!c.beyond_lookback);
        let credits = c
            .feed_credits
            .iter()
            .map(|credit| (credit.feed_id, credit.feed_title.as_str(), credit.credit))
            .collect::<Vec<_>>();
        assert_eq!(
            credits,
            [(7, "Example Feed", 0.25), (9, "Second Feed", 0.25)]
        );
        assert!(c.in_prompt && c.in_rebuild);

        // Rank 60 falls out of the 60-line prompt block but stays in the
        // 200-item rebuild set; rank 200 is in neither.
        let c = contribution(&rating, Some(&article), true, Some(60), now, &config);
        assert!(!c.in_prompt && c.in_rebuild);
        let c = contribution(&rating, Some(&article), true, Some(200), now, &config);
        assert!(!c.in_prompt && !c.in_rebuild);

        // Not-for-me 120 days ago: decay 0.25, weight −0.25, one direct feed
        // gets the whole (negative) credit; no embedding → no neighbour weight.
        let rating = rated(2, "not_for_me", -1.0, "2026-05-06T00:00:00Z");
        let article = crate::epub::fixtures::article(2, 1002, "One feed");
        let c = contribution(&rating, Some(&article), false, Some(0), now, &config);
        assert!((c.decay - 0.25).abs() < 1e-9);
        assert_eq!(c.neighbour_weight, None);
        assert_eq!(c.feed_credits.len(), 1);
        assert!((c.feed_credits[0].credit + 0.25).abs() < 1e-9);

        // Cleared verdicts contribute nothing anywhere.
        let rating = rated(3, "cleared", 0.0, "2026-09-02T00:00:00Z");
        let c = contribution(&rating, Some(&article), true, None, now, &config);
        assert_eq!(c.neighbour_weight, None);
        assert!(c.feed_credits.is_empty());
        assert!(!c.in_prompt && !c.in_rebuild);

        // Older than the 180-day lookback: flagged, weight still shown.
        let rating = rated(4, "good", 0.35, "2026-01-01T00:00:00Z");
        let c = contribution(&rating, Some(&article), true, Some(1), now, &config);
        assert!(c.beyond_lookback);
        assert!(c.neighbour_weight.is_some());
    }

    #[test]
    fn filters_parse_leniently_and_round_trip_into_hrefs() {
        let filters = Filters::parse(RatingsQuery {
            tab: Some("events".into()),
            label: Some("bogus".into()),
            source: Some("cli".into()),
            feed: Some("x".into()),
            q: Some("  Postgres ".into()),
            user: None,
            from: Some("2026-01-01".into()),
            to: Some("not a date".into()),
            page: Some("3".into()),
        });
        assert_eq!(filters.tab, "events");
        assert_eq!(filters.label, None);
        assert_eq!(filters.source.as_deref(), Some("cli"));
        assert_eq!(filters.feed, None);
        assert_eq!(filters.q.as_deref(), Some("Postgres"));
        assert_eq!(filters.from.as_deref(), Some("2026-01-01"));
        assert_eq!(filters.to, None);
        assert_eq!(filters.page, 3);
        assert_eq!(
            filters.href(Some(2)),
            "/dashboard/ratings?tab=events&source=cli&q=Postgres&from=2026-01-01&page=2"
        );
        assert_eq!(Filters::parse(RatingsQuery::default()).tab, "current");
    }

    async fn seed_article(db: &Db, id: ArticleId, entry_id: i64, title: &str) -> ArticleId {
        let article = crate::epub::fixtures::article(id, entry_id, title);
        db.upsert_entry(&Entry {
            id: article.best_entry_id,
            feed_id: article.feed_id,
            feed_title: Some(article.feed_title.clone()),
            category: article.category.clone(),
            title: article.title.clone(),
            url: article.url.clone(),
            canonical_url: Some(article.canonical_url.clone()),
            author: article.author.clone(),
            published_at: article.published_at,
            comments_url: article.comments_url.clone(),
            raw_content: article.content_html.clone(),
            fetched_at: article.first_seen,
        })
        .await
        .unwrap();
        db.upsert_article(&article).await.unwrap()
    }

    async fn seed_event(
        db: &Db,
        article_id: ArticleId,
        label: &str,
        value: f64,
        source: &str,
        user_id: Option<i64>,
        event_at: &str,
    ) -> i64 {
        db.append_rating_event(&RatingEvent {
            id: 0,
            user_id,
            article_id,
            issue_date: None,
            kind: "explicit".into(),
            source: source.into(),
            label: label.into(),
            value,
            note: Some(format!("note for {article_id}")),
            event_at: event_at.parse().unwrap(),
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn neighbour_usage_counts_candidates_and_selected_ones() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        for id in 1..=5 {
            seed_article(&db, id, 1000 + id, &format!("Article {id}")).await;
        }
        let run_id = db
            .start_run("2026-09-03".parse().unwrap(), Timestamp::now())
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET status = 'ok' WHERE id = ?")
            .bind(run_id)
            .execute(db.pool())
            .await
            .unwrap();
        let neighbours = |ids: &[ArticleId]| {
            let list = ids
                .iter()
                .map(|id| {
                    format!(
                        r#"{{"article_id":{id},"label":"loved","cos":0.8,"title":"Article {id}"}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"v":1,"neighbours":[{list}]}}"#)
        };
        for (article_id, stage, json) in [
            (3, "selected", neighbours(&[1, 2])),
            (4, "assessed", neighbours(&[1])),
            (5, "eligible", r#"{"v":1,"neighbours":[]}"#.to_string()),
        ] {
            sqlx::query(
                "INSERT INTO candidate_runs (run_id, article_id, stage, signals_json)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(run_id)
            .bind(article_id)
            .bind(stage)
            .bind(json)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let usage = neighbour_usage(&db, run_id).await.unwrap();
        assert_eq!(
            usage.get(&1),
            Some(&NeighbourUse {
                total: 2,
                selected: 1
            })
        );
        assert_eq!(
            usage.get(&2),
            Some(&NeighbourUse {
                total: 1,
                selected: 1
            })
        );
        assert_eq!(usage.get(&3), None);
        let last = last_real_run(&db).await.unwrap().unwrap();
        assert_eq!(last.id, run_id);
        assert_eq!(last.date, "2026-09-03");
    }

    async fn login_cookie(app: &axum::Router, username: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.44")
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

    async fn get(app: &axum::Router, uri: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ratings_page_renders_current_and_events_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let admin = users::add(&db, "tyler", "correct horse battery", true)
            .await
            .unwrap();
        let first = seed_article(&db, 1, 1001, "Postgres failover story").await;
        let second = seed_article(&db, 2, 1002, "A listicle").await;
        let third = seed_article(&db, 3, 1003, "Cleared later").await;
        seed_event(
            &db,
            first,
            "good",
            0.35,
            "cli",
            None,
            "2026-08-01T00:00:00Z",
        )
        .await;
        seed_event(
            &db,
            first,
            "loved",
            1.0,
            "dashboard",
            Some(admin.id),
            "2026-08-20T00:00:00Z",
        )
        .await;
        seed_event(
            &db,
            second,
            "not_for_me",
            -1.0,
            "epub",
            None,
            "2026-08-21T00:00:00Z",
        )
        .await;
        seed_event(
            &db,
            third,
            "loved",
            1.0,
            "cli",
            None,
            "2026-08-22T00:00:00Z",
        )
        .await;
        seed_event(
            &db,
            third,
            "cleared",
            0.0,
            "cli",
            None,
            "2026-08-23T00:00:00Z",
        )
        .await;
        // The first article has an embedding of the configured shape.
        let config = Config::default();
        let blob =
            crate::curate::embedding::encode_blob(&vec![0.01; config.voyage.output_dimension])
                .unwrap();
        sqlx::query(
            "INSERT INTO article_embeddings (article_id, model, dimension, input_hash, embedding, created_at)
             VALUES (?, ?, ?, 'hash', ?, '2026-08-20T00:00:00Z')",
        )
        .bind(first)
        .bind(&config.voyage.model)
        .bind(config.voyage.output_dimension as i64)
        .bind(blob)
        .execute(db.pool())
        .await
        .unwrap();
        let state = AppState::new(db, config, None);
        let app = crate::server::router(state);
        let cookie = login_cookie(&app, "tyler", "correct horse battery").await;

        let current = get(&app, "/dashboard/ratings", Some(&cookie)).await;
        assert_eq!(current.status(), StatusCode::OK);
        let body = text(current).await;
        assert!(body.contains("1 rated articles with embeddings"));
        assert!(body.contains("1 rated articles have no embedding"));
        assert!(body.contains("Postgres failover story"));
        assert!(body.contains("A listicle"));
        assert!(body.contains("Cleared later"));
        assert!(body.contains(">tyler<"));
        assert!(body.contains("no embedding"));
        assert!(body.contains("note for 1"));
        assert!(body.contains("How ratings enter the algorithm"));
        assert!(body.contains("/dashboard/articles/1"));
        assert!(body.contains(r#"name="note""#));
        assert!(body.contains("/dashboard/settings#curation.feedback"));

        let filtered = get(&app, "/dashboard/ratings?label=down", Some(&cookie)).await;
        let body = text(filtered).await;
        assert!(body.contains("A listicle"));
        assert!(!body.contains("Postgres failover story"));
        let searched = get(&app, "/dashboard/ratings?q=postgres", Some(&cookie)).await;
        let body = text(searched).await;
        assert!(body.contains("Postgres failover story"));
        assert!(!body.contains("A listicle"));

        let events = get(&app, "/dashboard/ratings?tab=events", Some(&cookie)).await;
        assert_eq!(events.status(), StatusCode::OK);
        let body = text(events).await;
        assert_eq!(body.matches(r#"class="superseded""#).count(), 2, "{body}");
        assert!(body.contains("e-reader link"));
        let by_source = get(
            &app,
            "/dashboard/ratings?tab=events&source=dashboard",
            Some(&cookie),
        )
        .await;
        let body = text(by_source).await;
        assert!(body.contains("Postgres failover story"));
        assert!(!body.contains("A listicle"));
        let by_date = get(
            &app,
            "/dashboard/ratings?tab=events&from=2026-08-22&to=2026-08-22",
            Some(&cookie),
        )
        .await;
        let body = text(by_date).await;
        assert!(body.contains("Cleared later"));
        assert!(!body.contains("A listicle"));
        let by_user = get(
            &app,
            "/dashboard/ratings?tab=events&user=tyler",
            Some(&cookie),
        )
        .await;
        let body = text(by_user).await;
        assert!(body.contains("Postgres failover story"));
        assert!(!body.contains("A listicle"));
    }

    #[tokio::test]
    async fn ratings_page_is_admin_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = crate::server::router(AppState::new(db, Config::default(), None));
        let anonymous = get(&app, "/dashboard/ratings", None).await;
        assert_eq!(anonymous.status(), StatusCode::FOUND);
        assert_eq!(
            anonymous.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fdashboard%2Fratings"
        );
        let reader = login_cookie(&app, "reader", "correct horse battery").await;
        let forbidden = get(&app, "/dashboard/ratings?tab=events", Some(&reader)).await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    }
}
