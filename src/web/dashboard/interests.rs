//! Dashboard: standing interests and their rating-derived weights.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use serde::Deserialize;

use super::jobs::set_flash;
use crate::curate::signals;
use crate::interests as interest_store;
use crate::server::AppState;
use crate::types::ArticleId;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError, encode_component, take_flash};

const PATH: &str = "/dashboard/interests";
const SORTS: [&str; 4] = ["weight", "name", "matches", "added"];

/// Routes contributed by the interests page.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(PATH, get(index).post(add))
        .route("/dashboard/interests/{id}/category", post(set_category))
        .route("/dashboard/interests/{id}/delete", post(delete))
}

#[derive(Debug, Default, Deserialize)]
struct InterestsQuery {
    category: Option<String>,
    sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddForm {
    name: String,
    #[serde(default)]
    category: String,
}

#[derive(Debug, Deserialize)]
struct CategoryForm {
    #[serde(default)]
    category: String,
}

#[derive(Debug)]
struct CategoryOption {
    name: String,
}

#[derive(Debug)]
struct InterestRow {
    id: i64,
    name: String,
    href: String,
    category: Option<String>,
    category_href: String,
    weight: String,
    weight_value: f64,
    up: String,
    down: String,
    rated_matches: usize,
    matched_articles: i64,
    added: String,
    created_at: String,
}

#[derive(Template)]
#[template(path = "dashboard/interests.html")]
struct InterestsTemplate {
    page: Page,
    rows: Vec<InterestRow>,
    categories: Vec<CategoryOption>,
    selected_category: String,
    selected_sort: String,
    sorts: Vec<&'static str>,
    total: usize,
    uncategorized: usize,
    lookback_days: i64,
    half_life_days: String,
    affinity_gate: String,
    attributable: usize,
    affinity_full: usize,
    jobs_enabled: bool,
}

async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<InterestsQuery>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated { next: PATH.into() })?;
    let config = state.config();
    let ranking = &config.curation.ranking;
    let db = &state.db;
    let now = Timestamp::now();
    let interests = interest_store::list(db).await.map_err(WebError::Internal)?;
    let total = interests.len();
    let uncategorized = interests
        .iter()
        .filter(|interest| interest.category.is_none())
        .count();

    let mut category_names = interests
        .iter()
        .filter_map(|interest| interest.category.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    category_names.sort_by(|left, right| {
        left.to_lowercase()
            .cmp(&right.to_lowercase())
            .then_with(|| left.cmp(right))
    });
    let categories = category_names
        .iter()
        .map(|name| CategoryOption { name: name.clone() })
        .collect::<Vec<_>>();

    let selected_category = query
        .category
        .as_deref()
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    let selected_sort = query
        .sort
        .as_deref()
        .filter(|sort| SORTS.contains(sort))
        .unwrap_or("weight")
        .to_string();

    let ratings = db.current_ratings(ranking.rating_lookback_days).await?;
    let rated_ids = ratings
        .iter()
        .map(|rating| rating.article_id)
        .collect::<Vec<_>>();
    let matches = interest_store::matches_for_articles(db, &rated_ids)
        .await
        .map_err(WebError::Internal)?;
    let rated = ratings
        .iter()
        .map(|rating| {
            let age_days = (now.as_second() - rating.event_at.as_second()).max(0) as f64 / 86_400.0;
            (
                rating.article_id,
                rating.value,
                signals::decay(age_days, ranking.rating_half_life_days),
            )
        })
        .collect::<Vec<(ArticleId, f64, f64)>>();
    let matched = matches
        .iter()
        .map(|row| (row.article_id, row.interest_id, row.z))
        .collect::<Vec<_>>();
    let rates = interest_store::rates(&rated, &matched);
    let counts = interest_store::match_counts(db)
        .await
        .map_err(WebError::Internal)?;

    let mut rows = interests
        .into_iter()
        .filter(|interest| match selected_category.as_str() {
            "" => true,
            "uncategorized" => interest.category.is_none(),
            category => interest
                .category
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(category)),
        })
        .map(|interest| {
            let rate = rates
                .by_interest
                .get(&interest.id)
                .copied()
                .unwrap_or_default();
            let category_href = interest
                .category
                .as_deref()
                .map(|category| format!("{PATH}?category={}", encode_component(category)))
                .unwrap_or_else(|| format!("{PATH}?category=uncategorized"));
            InterestRow {
                id: interest.id,
                href: interest_store::articles_href(&interest.name),
                name: interest.name,
                category: interest.category,
                category_href,
                weight: if rate.n > 0 {
                    format!("{:.2}", rate.weight())
                } else {
                    "—".into()
                },
                weight_value: rate.weight(),
                up: format!("{:.2}", rate.up),
                down: format!("{:.2}", rate.down),
                rated_matches: rate.n,
                matched_articles: counts.get(&interest.id).copied().unwrap_or(0),
                added: super::fmt_stored_time(Some(&interest.created_at), &config),
                created_at: interest.created_at,
            }
        })
        .collect::<Vec<_>>();
    sort_rows(&mut rows, &selected_sort);

    let mut page = Page::new("Interests", Some(viewer), "interests");
    page.flash = take_flash(&session).await?;
    Ok(Html(InterestsTemplate {
        page,
        rows,
        categories,
        selected_category,
        selected_sort,
        sorts: SORTS.to_vec(),
        total,
        uncategorized,
        lookback_days: ranking.rating_lookback_days,
        half_life_days: format!("{}", ranking.rating_half_life_days),
        affinity_gate: format!(
            "{:.2}",
            signals::gate(
                rates.attributable,
                ranking.affinity_floor,
                ranking.affinity_full
            )
        ),
        attributable: rates.attributable,
        affinity_full: ranking.affinity_full,
        jobs_enabled: config.server.jobs_enabled,
    })
    .into_response())
}

fn sort_rows(rows: &mut [InterestRow], sort: &str) {
    rows.sort_by(|left, right| {
        let selected = match sort {
            "name" => Ordering::Equal,
            "matches" => right.matched_articles.cmp(&left.matched_articles),
            "added" => right.created_at.cmp(&left.created_at),
            _ => right
                .weight_value
                .partial_cmp(&left.weight_value)
                .unwrap_or(Ordering::Equal),
        };
        selected.then_with(|| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        })
    });
}

fn form_category(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

async fn add(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Form(form): Form<AddForm>,
) -> Result<Response, WebError> {
    match interest_store::add(
        &state.db,
        &form.name,
        form_category(&form.category),
        Timestamp::now(),
    )
    .await
    {
        Ok(interest_store::AddOutcome::Added(_)) => {
            set_flash(&session, "success", format!("Added {}.", form.name.trim())).await?;
        }
        Ok(interest_store::AddOutcome::Duplicate) => {
            set_flash(
                &session,
                "error",
                format!("{} already exists.", form.name.trim()),
            )
            .await?;
        }
        Err(error) => set_flash(&session, "error", error.to_string()).await?,
    }
    Ok(Redirect::to(PATH).into_response())
}

async fn set_category(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
    Form(form): Form<CategoryForm>,
) -> Result<Response, WebError> {
    interest_store::set_category(
        &state.db,
        id,
        form_category(&form.category),
        Timestamp::now(),
    )
    .await
    .map_err(WebError::Internal)?;
    set_flash(&session, "success", "Category saved.".into()).await?;
    Ok(Redirect::to(PATH).into_response())
}

async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM interests WHERE id = ?")
        .bind(id)
        .fetch_optional(state.db.pool())
        .await
        .map_err(super::db_err)?;
    interest_store::delete(&state.db, id)
        .await
        .map_err(WebError::Internal)?;
    set_flash(
        &session,
        "success",
        name.map(|name| format!("Deleted {name}."))
            .unwrap_or_else(|| "Interest already deleted.".into()),
    )
    .await?;
    Ok(Redirect::to(PATH).into_response())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::interests::AddOutcome;
    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, login_cookie, response_text, seed,
    };

    async fn post_form(
        app: &axum::Router,
        uri: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn page_sorts_rating_weights_descending_and_is_admin_only() {
        let seed = seed().await;
        let now = Timestamp::now();
        let AddOutcome::Added(rust) = interest_store::add(&seed.db, "Rust", Some("Software"), now)
            .await
            .unwrap()
        else {
            unreachable!();
        };
        interest_store::add(&seed.db, "Cooking", None, now)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO article_interests (article_id, interest_id, cos, z, run_id)
             VALUES (1, ?, 0.8, 3.0, ?)",
        )
        .bind(rust)
        .bind(seed.run_id)
        .execute(seed.db.pool())
        .await
        .unwrap();

        let app = app_with_users(&seed.db).await;
        let body = assert_admin_only(&app, PATH).await;
        let table = body.split_once("<tbody>").unwrap().1;
        assert!(table.find(">Rust</a>").unwrap() < table.find(">Cooking</a>").unwrap());
        assert!(
            table.contains("/dashboard/articles?interest=Rust"),
            "{table}"
        );
        assert!(body.contains("affinity gate"), "{body}");

        let reader = login_cookie(&app, "reader", "correct horse battery").await;
        for (uri, body) in [
            (PATH.to_string(), "name=New&category="),
            (
                format!("/dashboard/interests/{rust}/category"),
                "category=Other",
            ),
            (format!("/dashboard/interests/{rust}/delete"), ""),
        ] {
            let forbidden = post_form(&app, &uri, Some(&reader), body).await;
            assert_eq!(forbidden.status(), StatusCode::FORBIDDEN, "{uri}");
        }
    }

    #[tokio::test]
    async fn add_reports_a_case_insensitive_duplicate() {
        let seed = seed().await;
        interest_store::add(&seed.db, "Rust", None, Timestamp::now())
            .await
            .unwrap();
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let response = post_form(&app, PATH, Some(&admin), "name=rust&category=").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let body =
            response_text(crate::web::dashboard::tests::get(&app, PATH, Some(&admin)).await).await;
        assert!(body.contains("rust already exists."), "{body}");
        assert_eq!(interest_store::list(&seed.db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_action_removes_matches_and_the_embedding() {
        let seed = seed().await;
        let AddOutcome::Added(id) = interest_store::add(&seed.db, "Rust", None, Timestamp::now())
            .await
            .unwrap()
        else {
            unreachable!();
        };
        sqlx::query(
            "INSERT INTO article_interests (article_id, interest_id, cos, z)
             VALUES (1, ?, 0.8, 2.0)",
        )
        .bind(id)
        .execute(seed.db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO interest_embeddings
             (interest, model, dimension, embedding, created_at)
             VALUES ('Rust', 'test', 1, X'00000000', '2026-09-12T00:00:00Z')",
        )
        .execute(seed.db.pool())
        .await
        .unwrap();
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let response = post_form(
            &app,
            &format!("/dashboard/interests/{id}/delete"),
            Some(&admin),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let interests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM interests")
            .fetch_one(seed.db.pool())
            .await
            .unwrap();
        let matches: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM article_interests")
            .fetch_one(seed.db.pool())
            .await
            .unwrap();
        let embeddings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM interest_embeddings")
            .fetch_one(seed.db.pool())
            .await
            .unwrap();
        assert_eq!((interests, matches, embeddings), (0, 0, 0));
    }
}
