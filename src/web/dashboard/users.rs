//! Dashboard: the read-only Users page (`/dashboard/users`, plan §6.1).

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_login::tower_sessions::Session;
use sqlx::Row as _;

use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Flash, Html, Page, WebError, format_time, take_flash};

use super::{db_err, fmt_stored_time};

#[derive(Debug)]
struct UserLine {
    username: String,
    role: String,
    disabled: bool,
    created: String,
    last_login: String,
    open_sessions: i64,
}

#[derive(Debug)]
struct AccessRequestLine {
    id: i64,
    email: String,
    reason: String,
    requested: String,
}

#[derive(Template)]
#[template(path = "dashboard/users.html")]
struct UsersTemplate {
    page: Page,
    requests: Vec<AccessRequestLine>,
    users: Vec<UserLine>,
}

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/users", get(index))
        .route("/dashboard/users/requests/{id}/done", post(mark_done))
}

async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Html<UsersTemplate>, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
    let requests = sqlx::query(
        "SELECT id, email, reason, requested_at FROM account_requests
         WHERE status = 'open' ORDER BY requested_at DESC, id DESC",
    )
    .fetch_all(state.db.pool())
    .await
    .map_err(db_err)?
    .into_iter()
    .map(|row| AccessRequestLine {
        id: row.get("id"),
        email: row.get("email"),
        reason: row.get::<Option<String>, _>("reason").unwrap_or_default(),
        requested: fmt_stored_time(Some(row.get::<String, _>("requested_at").as_str()), &config),
    })
    .collect();
    let users = crate::web::users::list(&state.db)
        .await
        .map_err(WebError::Internal)?
        .into_iter()
        .map(|row| UserLine {
            username: row.user.username,
            role: row.user.role.to_string(),
            disabled: row.user.disabled,
            created: format_time(row.user.created_at, &config),
            last_login: row
                .user
                .last_login_at
                .map(|at| format_time(at, &config))
                .unwrap_or_else(|| "never".into()),
            open_sessions: row.open_sessions,
        })
        .collect();
    let mut page = Page::new("Users", viewer, "users");
    page.flash = take_flash(&session).await?;
    Ok(Html(UsersTemplate {
        page,
        requests,
        users,
    }))
}

async fn mark_done(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/dashboard/users".into(),
    })?;
    let result = sqlx::query(
        "UPDATE account_requests SET status = 'done', handled_at = ?, handled_by = ?
         WHERE id = ? AND status = 'open'",
    )
    .bind(crate::db::fmt_ts(jiff::Timestamp::now()))
    .bind(viewer.id)
    .bind(id)
    .execute(state.db.pool())
    .await
    .map_err(db_err)?;
    if result.rows_affected() == 0 {
        return Err(WebError::NotFound);
    }
    session
        .insert(
            "flash",
            Flash {
                kind: "success".into(),
                text: "Access request marked done.".into(),
            },
        )
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    Ok(Redirect::to("/dashboard/users").into_response())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use sqlx::Row as _;
    use tower::ServiceExt;

    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, get, login_cookie, response_text, seed,
    };

    #[tokio::test]
    async fn users_page_is_admin_only_and_lists_accounts_and_sessions() {
        let seed = seed().await;
        sqlx::query(
            "INSERT INTO account_requests (email, reason, requested_at)
             VALUES ('reader@example.com', 'Daily commute', '2026-09-05T12:00:00Z')",
        )
        .execute(seed.db.pool())
        .await
        .unwrap();
        let app = app_with_users(&seed.db).await;
        let body = assert_admin_only(&app, "/dashboard/users").await;

        assert!(body.contains("<h1>Users</h1>"), "{body}");
        assert!(body.contains("1 open access request"), "{body}");
        assert!(body.contains("reader@example.com"), "{body}");
        assert!(body.contains("Daily commute"), "{body}");
        assert!(body.contains("Mark done"), "{body}");
        assert!(body.contains("reader"), "{body}");
        assert!(body.contains("admin"), "{body}");
        assert!(body.contains("Open sessions"), "{body}");
        assert!(body.contains("daily-epub users"), "{body}");
    }

    #[tokio::test]
    async fn marking_a_request_done_hides_it_from_the_open_list() {
        let seed = seed().await;
        let request_id = sqlx::query(
            "INSERT INTO account_requests (email, reason, requested_at)
             VALUES ('done@example.com', NULL, '2026-09-05T12:00:00Z') RETURNING id",
        )
        .fetch_one(seed.db.pool())
        .await
        .unwrap()
        .get::<i64, _>("id");
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/dashboard/users/requests/{request_id}/done"))
                    .header(header::COOKIE, &admin)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/dashboard/users"
        );

        let row =
            sqlx::query("SELECT status, handled_at, handled_by FROM account_requests WHERE id = ?")
                .bind(request_id)
                .fetch_one(seed.db.pool())
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("status"), "done");
        assert!(row.get::<Option<String>, _>("handled_at").is_some());
        assert!(row.get::<Option<i64>, _>("handled_by").is_some());

        let page = get(&app, "/dashboard/users", Some(&admin)).await;
        let body = response_text(page).await;
        assert!(body.contains("0 open access requests"), "{body}");
        assert!(!body.contains("done@example.com"), "{body}");
    }
}
