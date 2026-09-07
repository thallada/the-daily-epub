//! Dashboard: user accounts and access requests (`/dashboard/users`, plan §6.1).

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Form, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_login::tower_sessions::Session;
use serde::Deserialize;
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
    suggested_username: String,
    reason: String,
    requested: String,
}

#[derive(Template)]
#[template(path = "dashboard/users.html")]
struct UsersTemplate {
    page: Page,
    requests: Vec<AccessRequestLine>,
    users: Vec<UserLine>,
    mail_configured: bool,
}

#[derive(Debug, Deserialize)]
struct ApproveForm {
    username: String,
}

#[derive(Debug)]
struct OpenRequest {
    email: String,
}

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/users", get(index))
        .route("/dashboard/users/requests/{id}/approve", post(approve))
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
    .map(|row| {
        let email: String = row.get("email");
        AccessRequestLine {
            id: row.get("id"),
            suggested_username: suggested_username(&email),
            email,
            reason: row.get::<Option<String>, _>("reason").unwrap_or_default(),
            requested: fmt_stored_time(
                Some(row.get::<String, _>("requested_at").as_str()),
                &config,
            ),
        }
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
        mail_configured: state.mailer.is_some(),
    }))
}

async fn approve(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
    Form(form): Form<ApproveForm>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/dashboard/users".into(),
    })?;
    let request = open_request(&state, id).await?;
    let Some(mailer) = state.mailer.as_ref() else {
        set_flash(
            &session,
            "error",
            "Email is not configured; create the account with the CLI instead".into(),
        )
        .await?;
        return Ok(Redirect::to("/dashboard/users").into_response());
    };

    let username = form.username.trim();
    let (user, temporary_password) =
        match crate::web::users::add_with_temporary_password(&state.db, username).await {
            Ok(created) => created,
            Err(error) => {
                set_flash(&session, "error", error.to_string()).await?;
                return Ok(Redirect::to("/dashboard/users").into_response());
            }
        };
    let sign_in_url = format!(
        "{}/login",
        state.config().server.public_url.trim_end_matches('/')
    );
    let message = crate::mail::Message {
        to: request.email.clone(),
        subject: "Your Daily EPUB account".into(),
        body: format!(
            "Username: {username}\nTemporary password: {temporary_password}\nSign in: {sign_in_url}\n\nImportant: you will be asked to choose a new password when you sign in.\n"
        ),
    };
    if let Err(error) = mailer.send(message).await {
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(user.id)
            .execute(state.db.pool())
            .await
            .map_err(db_err)?;
        set_flash(
            &session,
            "error",
            format!("Could not email {}: {error}", request.email),
        )
        .await?;
        return Ok(Redirect::to("/dashboard/users").into_response());
    }

    finish_request(&state, id, viewer.id).await?;
    set_flash(
        &session,
        "success",
        format!("Created {username} and emailed {}.", request.email),
    )
    .await?;
    Ok(Redirect::to("/dashboard/users").into_response())
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
    finish_request(&state, id, viewer.id).await?;
    set_flash(&session, "success", "Access request marked done.".into()).await?;
    Ok(Redirect::to("/dashboard/users").into_response())
}

async fn open_request(state: &AppState, id: i64) -> Result<OpenRequest, WebError> {
    sqlx::query("SELECT email FROM account_requests WHERE id = ? AND status = 'open'")
        .bind(id)
        .fetch_optional(state.db.pool())
        .await
        .map_err(db_err)?
        .map(|row| OpenRequest {
            email: row.get("email"),
        })
        .ok_or(WebError::NotFound)
}

async fn finish_request(state: &AppState, id: i64, handled_by: i64) -> Result<(), WebError> {
    let result = sqlx::query(
        "UPDATE account_requests SET status = 'done', handled_at = ?, handled_by = ?
         WHERE id = ? AND status = 'open'",
    )
    .bind(crate::db::fmt_ts(jiff::Timestamp::now()))
    .bind(handled_by)
    .bind(id)
    .execute(state.db.pool())
    .await
    .map_err(db_err)?;
    if result.rows_affected() == 0 {
        return Err(WebError::NotFound);
    }
    Ok(())
}

async fn set_flash(session: &Session, kind: &str, text: String) -> Result<(), WebError> {
    session
        .insert(
            "flash",
            Flash {
                kind: kind.into(),
                text,
            },
        )
        .await
        .map_err(|error| WebError::Internal(error.into()))
}

fn suggested_username(email: &str) -> String {
    email
        .split_once('@')
        .map_or(email, |(local, _)| local)
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .filter(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::server::{AppState, router};
    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, get, login_cookie, response_text, seed,
    };

    async fn insert_request(db: &crate::db::Db, email: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO account_requests (email, requested_at)
             VALUES (?, '2026-09-05T12:00:00Z') RETURNING id",
        )
        .bind(email)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    async fn app_with_mailer(db: &crate::db::Db, mailer: crate::mail::Mailer) -> axum::Router {
        crate::web::users::add(db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        crate::web::users::add(db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let mut config = Config::default();
        config.server.public_url = "https://daily.example/".into();
        let mut state = AppState::new(db.clone(), config, None);
        state.mailer = Some(mailer);
        router(state)
    }

    async fn post_approve(
        app: &axum::Router,
        request_id: i64,
        cookie: &str,
        username: &str,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/dashboard/users/requests/{request_id}/approve"))
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(format!("username={username}")))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

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
        assert!(body.contains("value=\"reader\""), "{body}");
        assert!(body.contains(">Approve</button>"), "{body}");
        assert!(body.contains("Email is not configured"), "{body}");
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

    #[test]
    fn username_suggestions_normalize_the_email_local_part() {
        assert_eq!(
            suggested_username("Some.Name+news@example.com"),
            "somenamenews"
        );
        assert_eq!(suggested_username("R_E-A_D_E_R@example.com"), "r_e-a_d_e_r");
        assert_eq!(suggested_username("...@example.com"), "");
    }

    #[tokio::test]
    async fn approval_creates_a_flagged_user_emails_the_password_and_finishes_request() {
        let seed = seed().await;
        let request_id = insert_request(&seed.db, "new-reader@example.com").await;
        let (mailer, messages) = crate::mail::Mailer::recording();
        let app = app_with_mailer(&seed.db, mailer).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_approve(&app, request_id, &admin, "new_reader").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/dashboard/users"
        );

        let user = crate::web::users::find_by_username(&seed.db, "new_reader")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.role, crate::web::users::Role::User);
        assert!(user.must_change_password);
        let request =
            sqlx::query("SELECT status, handled_at, handled_by FROM account_requests WHERE id = ?")
                .bind(request_id)
                .fetch_one(seed.db.pool())
                .await
                .unwrap();
        assert_eq!(request.get::<String, _>("status"), "done");
        assert!(request.get::<Option<String>, _>("handled_at").is_some());
        assert!(request.get::<Option<i64>, _>("handled_by").is_some());

        let message = {
            let messages = messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(messages.len(), 1);
            messages[0].clone()
        };
        assert_eq!(message.to, "new-reader@example.com");
        assert_eq!(message.subject, "Your Daily EPUB account");
        assert!(message.body.contains("Username: new_reader"));
        assert!(
            message
                .body
                .contains("Sign in: https://daily.example/login")
        );
        assert!(message.body.contains("choose a new password"));
        let password = message
            .body
            .lines()
            .find_map(|line| line.strip_prefix("Temporary password: "))
            .unwrap();
        assert_eq!(password.len(), 20);
        assert!(crate::web::users::verify_password(
            &user.password_hash,
            password
        ));

        let page = get(&app, "/dashboard/users", Some(&admin)).await;
        let body = response_text(page).await;
        assert!(
            body.contains("Created new_reader and emailed new-reader@example.com."),
            "{body}"
        );
    }

    #[tokio::test]
    async fn approval_without_mail_refuses_without_changing_request() {
        let seed = seed().await;
        let request_id = insert_request(&seed.db, "no-mail@example.com").await;
        let app = app_with_users(&seed.db).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_approve(&app, request_id, &admin, "no_mail").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            crate::web::users::find_by_username(&seed.db, "no_mail")
                .await
                .unwrap()
                .is_none()
        );
        let status: String = sqlx::query_scalar("SELECT status FROM account_requests WHERE id = ?")
            .bind(request_id)
            .fetch_one(seed.db.pool())
            .await
            .unwrap();
        assert_eq!(status, "open");
        let page = get(&app, "/dashboard/users", Some(&admin)).await;
        let body = response_text(page).await;
        assert!(
            body.contains("Email is not configured; create the account with the CLI instead"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn failed_approval_email_deletes_the_user_and_leaves_request_open() {
        let seed = seed().await;
        let request_id = insert_request(&seed.db, "failure@example.com").await;
        let app = app_with_mailer(&seed.db, crate::mail::Mailer::failing()).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post_approve(&app, request_id, &admin, "delivery_failure").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            crate::web::users::find_by_username(&seed.db, "delivery_failure")
                .await
                .unwrap()
                .is_none()
        );
        let status: String = sqlx::query_scalar("SELECT status FROM account_requests WHERE id = ?")
            .bind(request_id)
            .fetch_one(seed.db.pool())
            .await
            .unwrap();
        assert_eq!(status, "open");
        let page = get(&app, "/dashboard/users", Some(&admin)).await;
        let body = response_text(page).await;
        assert!(
            body.contains("Could not email failure@example.com"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn approval_is_admin_only() {
        let seed = seed().await;
        let request_id = insert_request(&seed.db, "forbidden@example.com").await;
        let app = app_with_users(&seed.db).await;
        let reader = login_cookie(&app, "reader", "correct horse battery").await;

        let response = post_approve(&app, request_id, &reader, "forbidden").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            crate::web::users::find_by_username(&seed.db, "forbidden")
                .await
                .unwrap()
                .is_none()
        );
    }
}
