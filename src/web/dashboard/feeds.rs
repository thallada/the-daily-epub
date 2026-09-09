//! Dashboard: feed candidates (`/dashboard/feeds`, feed discovery plan §4
//! step 5).
//!
//! The discovery stage records the feeds behind aggregator-only articles in
//! `feed_candidates`; this page ranks the undecided ones by how likely the
//! operator is to enjoy them ([`discovery::score`]) and offers **Add** (which
//! subscribes through `POST /v1/feeds`) and **Dismiss**.
//!
//! Ranking is global, so the candidate view loads every undecided row, scores
//! it in Rust and paginates afterwards (plan §4 step 4). The decided views are
//! ordinary SQL pages.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use serde::Deserialize;
use sqlx::Row as _;

use super::jobs::set_flash;
use super::{Pager, allow_listed, db_err, dynamic_query, fmt_stored_time, page_number};
use crate::config::Config;
use crate::db::Db;
use crate::discovery::{self, ArticleEvidence, Candidate};
use crate::miniflux::{MinifluxCategory, MinifluxClient};
use crate::server::AppState;
use crate::types::ArticleId;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, Pagination, WebError, take_flash};

const CANDIDATES_PER_PAGE: u32 = 50;
/// `feed_candidates.status` values, allow-listed for `?status=`.
const STATUSES: [&str; 3] = ["candidate", "added", "dismissed"];
/// Articles named in the "Why" column.
const WHY_ARTICLES: usize = 3;
/// `kv` key remembering the category the last Add used (plan §3).
const LAST_CATEGORY_KEY: &str = "feed_discovery_last_category";
const PATH: &str = "/dashboard/feeds";

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(PATH, get(index))
        .route("/dashboard/feeds/{id}/add", post(add))
        .route("/dashboard/feeds/{id}/dismiss", post(dismiss))
}

#[derive(Debug, Default, Deserialize)]
struct FeedsQuery {
    status: Option<String>,
    page: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AddForm {
    category_id: i64,
}

/// One linked article, as named in the "Why" column.
#[derive(Debug)]
struct WhyArticle {
    id: ArticleId,
    title: String,
}

/// One table row.
#[derive(Debug)]
struct FeedRow {
    id: i64,
    /// The shrunk mean on 0–100, rounded.
    score: i64,
    feed_url: String,
    /// The candidate's title, or its feed URL when it has none.
    label: String,
    host: String,
    interests: Vec<String>,
    articles: Vec<WhyArticle>,
    article_count: usize,
    first_seen: String,
    last_seen: String,
    /// `last_seen` as a short date for the column; the full first/last
    /// timestamps sit in the cell's tooltip.
    seen: String,
    decided_at: String,
    /// The Miniflux web UI page for a candidate we subscribed to.
    miniflux_href: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard/feeds.html")]
struct FeedsTemplate {
    page: Page,
    status: String,
    rows: Vec<FeedRow>,
    candidate_count: i64,
    added_count: i64,
    dismissed_count: i64,
    categories: Vec<MinifluxCategory>,
    selected_category: i64,
    /// Why the category picker is empty, when it is; Add is disabled then.
    categories_error: Option<String>,
    pager: Pager,
}

async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<FeedsQuery>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let db = &state.db;
    let status = allow_listed(query.status.as_deref(), &STATUSES).unwrap_or("candidate");
    let page_no = page_number(query.page);

    // Candidates are ranked globally, so they are all loaded, scored, sorted
    // and only then paginated (plan §4 step 4).
    let (candidates, total, ranked_evidence) = if status == "candidate" {
        let all = discovery::all_candidates(db).await?;
        let evidence = load_evidence(db, &config, &all).await?;
        let mut ranked: Vec<(Candidate, f64)> = all
            .into_iter()
            .map(|candidate| {
                let score = discovery::score(&evidence_of(&candidate, &evidence));
                (candidate, score)
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| b.0.article_ids.len().cmp(&a.0.article_ids.len()))
                .then_with(|| b.0.last_seen.cmp(&a.0.last_seen))
        });
        let total = ranked.len() as i64;
        let offset = (page_no.saturating_sub(1) as usize) * CANDIDATES_PER_PAGE as usize;
        let page_rows: Vec<Candidate> = ranked
            .into_iter()
            .skip(offset)
            .take(CANDIDATES_PER_PAGE as usize)
            .map(|(candidate, _)| candidate)
            .collect();
        (page_rows, total, Some(evidence))
    } else {
        let (candidates, total) = discovery::list(db, status, page_no, CANDIDATES_PER_PAGE).await?;
        (candidates, total, None)
    };

    let evidence = match ranked_evidence {
        Some(evidence) => evidence,
        None => load_evidence(db, &config, &candidates).await?,
    };
    let titles = article_titles(db, &candidates).await?;
    let rows: Vec<FeedRow> = candidates
        .iter()
        .map(|candidate| row(candidate, &evidence, &titles, &config))
        .collect();

    // The picker is only ever used by the candidate view, so only it pays for
    // the categories call (plan §3).
    let (categories, categories_error) = if status == "candidate" {
        categories(&config).await
    } else {
        (Vec::new(), None)
    };
    let selected_category = selected_category(db, &categories).await?;

    let pager = Pager::new(
        Pagination {
            page: page_no,
            per_page: CANDIDATES_PER_PAGE,
            total,
        },
        PATH,
        &[("status", Some(status.to_string()))],
    );
    let mut page = Page::new("Feeds", viewer, "feeds");
    page.flash = take_flash(&session).await?;
    Ok(Html(FeedsTemplate {
        page,
        status: status.to_string(),
        rows,
        candidate_count: discovery::count(db, "candidate").await?,
        added_count: discovery::count(db, "added").await?,
        dismissed_count: discovery::count(db, "dismissed").await?,
        categories,
        selected_category,
        categories_error,
        pager,
    })
    .into_response())
}

async fn add(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
    Form(form): Form<AddForm>,
) -> Result<Response, WebError> {
    let config = state.config();
    let db = &state.db;
    let now = Timestamp::now();
    let Some(candidate) = discovery::candidate(db, id).await? else {
        set_flash(&session, "error", format!("No feed candidate {id}.")).await?;
        return Ok(Redirect::to(PATH).into_response());
    };
    let label = candidate
        .title
        .clone()
        .unwrap_or_else(|| candidate.feed_url.clone());
    let Some(client) = client(&config) else {
        set_flash(
            &session,
            "error",
            "Miniflux API key is not configured".into(),
        )
        .await?;
        return Ok(Redirect::to(PATH).into_response());
    };

    match client
        .create_feed(&candidate.feed_url, form.category_id)
        .await
    {
        Ok(feed_id) => {
            discovery::set_status(db, id, "added", Some(feed_id), now).await?;
            db.kv_set(LAST_CATEGORY_KEY, &form.category_id.to_string())
                .await?;
            set_flash(&session, "success", format!("Added {label}.")).await?;
        }
        // Already subscribed by other means: the operator's intent is met, so
        // the row is decided too (plan §2).
        Err(error) if error.is_duplicate_feed() => {
            discovery::set_status(db, id, "added", None, now).await?;
            set_flash(&session, "success", format!("{label}: {error}")).await?;
        }
        Err(error) => {
            set_flash(&session, "error", format!("{label}: {error}")).await?;
        }
    }
    Ok(Redirect::to(PATH).into_response())
}

async fn dismiss(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let db = &state.db;
    let Some(candidate) = discovery::candidate(db, id).await? else {
        set_flash(&session, "error", format!("No feed candidate {id}.")).await?;
        return Ok(Redirect::to(PATH).into_response());
    };
    discovery::set_status(db, id, "dismissed", None, Timestamp::now()).await?;
    let label = candidate.title.unwrap_or(candidate.feed_url);
    set_flash(&session, "success", format!("Dismissed {label}.")).await?;
    Ok(Redirect::to(PATH).into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A Miniflux client from `[miniflux]`, or `None` when there is no API key.
fn client(config: &Config) -> Option<MinifluxClient> {
    let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).ok()?;
    MinifluxClient::new(&config.miniflux, http).ok()
}

/// `GET /v1/categories` for the Add picker; the error is shown as a notice on
/// the page (not a flash: nothing was submitted) and disables Add.
async fn categories(config: &Config) -> (Vec<MinifluxCategory>, Option<String>) {
    let Some(client) = client(config) else {
        return (
            Vec::new(),
            Some("Miniflux API key is not configured, so feeds cannot be added.".into()),
        );
    };
    match client.categories().await {
        Ok(categories) => (categories, None),
        Err(error) => (
            Vec::new(),
            Some(format!("Could not load Miniflux categories: {error}")),
        ),
    }
}

/// The category the last Add used, when it still exists; else the first one.
async fn selected_category(db: &Db, categories: &[MinifluxCategory]) -> Result<i64, WebError> {
    let last = db
        .kv_get(LAST_CATEGORY_KEY)
        .await?
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|id| categories.iter().any(|category| category.id == *id));
    Ok(last
        .or_else(|| categories.first().map(|category| category.id))
        .unwrap_or_default())
}

fn union_ids(candidates: &[Candidate]) -> Vec<ArticleId> {
    let mut seen: HashSet<ArticleId> = HashSet::new();
    candidates
        .iter()
        .flat_map(|candidate| candidate.article_ids.iter().copied())
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Ranking evidence for every article linked to `candidates`, in one query.
async fn load_evidence(
    db: &Db,
    config: &Config,
    candidates: &[Candidate],
) -> Result<HashMap<ArticleId, ArticleEvidence>, WebError> {
    Ok(discovery::load_evidence(
        db,
        &union_ids(candidates),
        config.curation.ranking.rating_lookback_days,
    )
    .await?)
}

/// Titles for every linked article, including the ones with no evidence (which
/// the "Why" column still lists, last).
async fn article_titles(
    db: &Db,
    candidates: &[Candidate],
) -> Result<HashMap<ArticleId, String>, WebError> {
    let ids = union_ids(candidates);
    let mut titles = HashMap::new();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = dynamic_query(format!(
            "SELECT id, COALESCE(title, '') AS title FROM articles WHERE id IN ({placeholders})"
        ));
        for id in chunk {
            query = query.bind(*id);
        }
        for row in query.fetch_all(db.pool()).await.map_err(db_err)? {
            titles.insert(row.get("id"), row.get("title"));
        }
    }
    Ok(titles)
}

/// `Sep 7` for the current year, `Sep 7, 2025` otherwise, in the configured
/// time zone; the raw string when it does not parse.
fn compact_date(raw: &str, config: &Config) -> String {
    let (Ok(timestamp), Ok(tz)) = (raw.parse::<Timestamp>(), config.tz()) else {
        return raw.to_string();
    };
    let zoned = timestamp.to_zoned(tz.clone());
    let format = if zoned.year() == Timestamp::now().to_zoned(tz).year() {
        "%b %-d"
    } else {
        "%b %-d, %Y"
    };
    zoned.strftime(format).to_string()
}

fn evidence_of(
    candidate: &Candidate,
    evidence: &HashMap<ArticleId, ArticleEvidence>,
) -> Vec<ArticleEvidence> {
    candidate
        .article_ids
        .iter()
        .filter_map(|id| evidence.get(id).cloned())
        .collect()
}

fn row(
    candidate: &Candidate,
    evidence: &HashMap<ArticleId, ArticleEvidence>,
    titles: &HashMap<ArticleId, String>,
    config: &Config,
) -> FeedRow {
    let mut scored = evidence_of(candidate, evidence);
    let score = discovery::score(&scored);
    let interests = discovery::why(&scored);

    // Best evidence first; the articles nothing is known about come last, in
    // title order.
    scored.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.title.cmp(&b.title))
    });
    let mut unscored: Vec<WhyArticle> = candidate
        .article_ids
        .iter()
        .filter(|id| !evidence.contains_key(id))
        .map(|id| WhyArticle {
            id: *id,
            title: titles.get(id).cloned().unwrap_or_default(),
        })
        .collect();
    unscored.sort_by(|a, b| a.title.cmp(&b.title));
    let articles: Vec<WhyArticle> = scored
        .into_iter()
        .map(|item| WhyArticle {
            id: item.article_id,
            title: item.title,
        })
        .chain(unscored)
        .take(WHY_ARTICLES)
        .collect();

    FeedRow {
        id: candidate.id,
        score: score.round() as i64,
        feed_url: candidate.feed_url.clone(),
        label: candidate
            .title
            .clone()
            .unwrap_or_else(|| candidate.feed_url.clone()),
        host: candidate.host.clone(),
        interests,
        articles,
        article_count: candidate.article_ids.len(),
        first_seen: fmt_stored_time(Some(&candidate.first_seen), config),
        last_seen: fmt_stored_time(Some(&candidate.last_seen), config),
        seen: compact_date(&candidate.last_seen, config),
        decided_at: fmt_stored_time(candidate.decided_at.as_deref(), config),
        miniflux_href: candidate
            .miniflux_feed_id
            .map(|feed_id| config.miniflux.feed_url(feed_id)),
    }
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::server::router;
    use crate::web::dashboard::tests::{
        Seed, app_with_users, assert_admin_only, get, login_cookie, response_text, seed,
    };

    /// A candidate on `host` linked to `article_ids`.
    async fn candidate(db: &Db, title: &str, host: &str, article_ids: &[ArticleId]) -> i64 {
        let id = discovery::upsert_candidate(
            db,
            &format!("https://{host}/feed.xml"),
            host,
            Some(title),
            "2026-09-06T04:00:00Z".parse().unwrap(),
        )
        .await
        .unwrap();
        for article_id in article_ids {
            discovery::link_article(db, id, *article_id).await.unwrap();
        }
        id
    }

    async fn status_of(db: &Db, id: i64) -> (String, Option<i64>, Option<String>) {
        let candidate = discovery::candidate(db, id).await.unwrap().unwrap();
        (
            candidate.status,
            candidate.miniflux_feed_id,
            candidate.decided_at,
        )
    }

    /// A Miniflux stand-in answering `POST /v1/feeds` with `body`.
    async fn miniflux(status: StatusCode, body: serde_json::Value) -> String {
        let handler = move || {
            let body = body.clone();
            async move { (status, Json(body)) }
        };
        let app = Router::new().route("/v1/feeds", axum::routing::post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn app_with_miniflux(seed: &Seed, base_url: String) -> axum::Router {
        crate::web::users::add(&seed.db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        crate::web::users::add(&seed.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let mut config = Config::default();
        config.miniflux.base_url = base_url;
        config.miniflux.api_key = Some("test-key-never-logged".into());
        router(AppState::new(seed.db.clone(), config, None))
    }

    async fn post_form(app: &axum::Router, uri: &str, cookie: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("category_id=3"))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn feeds_page_is_admin_only_and_empty_without_candidates() {
        let seed = seed().await;
        let app = app_with_users(&seed.db).await;

        let body = assert_admin_only(&app, "/dashboard/feeds").await;

        assert!(body.contains("<h1>Feeds</h1>"), "{body}");
        assert!(body.contains("No feed candidates"), "{body}");
        assert!(body.contains("Candidates (0)"), "{body}");
        // No API key in the test config, so Add is disabled with a notice.
        assert!(
            body.contains("Miniflux API key is not configured, so feeds cannot be added."),
            "{body}"
        );
    }

    #[tokio::test]
    async fn candidates_are_listed_best_first() {
        let seed = seed().await;
        // Article 1 is rated `loved` (value 1.0); article 5 only ever reached
        // `triaged`, so it scores on its blend alone.
        candidate(&seed.db, "Weak Blog", "weak.example", &[5]).await;
        candidate(&seed.db, "Strong Blog", "strong.example", &[1]).await;
        let app = app_with_users(&seed.db).await;

        let body = assert_admin_only(&app, "/dashboard/feeds").await;

        assert!(body.contains("Candidates (2)"), "{body}");
        assert!(body.contains("Strong Blog"), "{body}");
        assert!(body.contains("Weak Blog"), "{body}");
        assert!(body.contains("https://strong.example/feed.xml"), "{body}");
        // The rated article is named in the Why column and links to it.
        assert!(body.contains("/dashboard/articles/1"), "{body}");
        assert!(body.contains("Gaussian Splatting"), "{body}");
        let strong = body.find("Strong Blog").unwrap();
        let weak = body.find("Weak Blog").unwrap();
        assert!(strong < weak, "{body}");
    }

    #[tokio::test]
    async fn dismiss_decides_the_row_and_redirects() {
        let seed = seed().await;
        let id = candidate(&seed.db, "Noise", "noise.example", &[2]).await;
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_form(&app, &format!("/dashboard/feeds/{id}/dismiss"), &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/dashboard/feeds"
        );

        let (status, feed_id, decided_at) = status_of(&seed.db, id).await;
        assert_eq!(status, "dismissed");
        assert_eq!(feed_id, None);
        assert!(decided_at.is_some());

        let body = response_text(get(&app, "/dashboard/feeds", Some(&admin)).await).await;
        assert!(body.contains("Dismissed Noise."), "{body}");
        assert!(body.contains("Candidates (0)"), "{body}");
        assert!(body.contains("Dismissed (1)"), "{body}");
    }

    #[tokio::test]
    async fn add_subscribes_and_records_the_miniflux_feed_id() {
        let seed = seed().await;
        let id = candidate(&seed.db, "Strong Blog", "strong.example", &[1]).await;
        let base_url = miniflux(StatusCode::CREATED, json!({"feed_id": 77})).await;
        let app = app_with_miniflux(&seed, base_url).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_form(&app, &format!("/dashboard/feeds/{id}/add"), &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let (status, feed_id, decided_at) = status_of(&seed.db, id).await;
        assert_eq!(status, "added");
        assert_eq!(feed_id, Some(77));
        assert!(decided_at.is_some());
        assert_eq!(
            seed.db.kv_get(LAST_CATEGORY_KEY).await.unwrap().as_deref(),
            Some("3")
        );

        let body = response_text(get(&app, "/dashboard/feeds", Some(&admin)).await).await;
        assert!(body.contains("Added Strong Blog."), "{body}");
        let added =
            response_text(get(&app, "/dashboard/feeds?status=added", Some(&admin)).await).await;
        assert!(added.contains("/feed/77/entries"), "{added}");
    }

    #[tokio::test]
    async fn a_duplicate_subscription_still_decides_the_row() {
        let seed = seed().await;
        let id = candidate(&seed.db, "Known Blog", "known.example", &[1]).await;
        let base_url = miniflux(
            StatusCode::BAD_REQUEST,
            json!({"error_message": "This feed already exists."}),
        )
        .await;
        let app = app_with_miniflux(&seed, base_url).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_form(&app, &format!("/dashboard/feeds/{id}/add"), &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let (status, feed_id, decided_at) = status_of(&seed.db, id).await;
        assert_eq!(status, "added");
        assert_eq!(feed_id, None);
        assert!(decided_at.is_some());

        let body = response_text(get(&app, "/dashboard/feeds", Some(&admin)).await).await;
        assert!(body.contains("This feed already exists."), "{body}");
    }
}
