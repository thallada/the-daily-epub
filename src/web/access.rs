//! Public account-access request page.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError};

const DESCRIPTION: &str =
    "Request an account to read complete issues online and download the EPUB and XTC editions.";

/// Fields accepted by the public access-request form.
#[derive(Debug, Deserialize)]
pub(super) struct AccessRequestForm {
    email: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    website: String,
}

#[derive(Template)]
#[template(path = "request_access.html")]
struct RequestAccessTemplate {
    page: Page,
    email: String,
    reason: String,
    error: String,
    submitted: bool,
}

fn template(viewer: Option<Viewer>) -> RequestAccessTemplate {
    RequestAccessTemplate {
        page: Page::new("Request access", viewer, "").with_description(DESCRIPTION),
        email: String::new(),
        reason: String::new(),
        error: String::new(),
        submitted: false,
    }
}

/// `GET /request-access`: explain reader accounts and show the request form.
pub(super) async fn page(auth: AuthSession) -> Response {
    Html(template(auth.user().await.map(Viewer::from))).into_response()
}

/// `POST /request-access`: validate and store (or update) an open request.
pub(super) async fn submit(
    State(state): State<AppState>,
    auth: AuthSession,
    Form(form): Form<AccessRequestForm>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    if !form.website.is_empty() {
        let mut view = template(viewer);
        view.submitted = true;
        return Ok(Html(view).into_response());
    }

    let email = form.email.trim();
    let reason = form.reason.trim();
    if let Some(error) = validate(email, reason) {
        let mut view = template(viewer);
        view.email = email.to_string();
        view.reason = reason.to_string();
        view.error = error.to_string();
        return Ok(Html(view).into_response());
    }

    let requested_at = crate::db::fmt_ts(jiff::Timestamp::now());
    sqlx::query(
        "INSERT INTO account_requests (email, reason, status, requested_at)
         VALUES (?, ?, 'open', ?)
         ON CONFLICT(email) WHERE status = 'open' DO UPDATE SET
             email = excluded.email,
             reason = excluded.reason,
             requested_at = excluded.requested_at",
    )
    .bind(email)
    .bind((!reason.is_empty()).then_some(reason))
    .bind(&requested_at)
    .execute(state.db.pool())
    .await
    .map_err(|error| WebError::Db(error.into()))?;

    let config = state.config();
    if let (Some(mailer), Some(to)) = (
        state.mailer.clone(),
        config
            .mail
            .notify_to
            .as_deref()
            .filter(|value| !value.trim().is_empty()),
    ) {
        let reason = if reason.is_empty() {
            "(no reason given)"
        } else {
            reason
        };
        let message = crate::mail::Message {
            to: to.to_string(),
            subject: format!("Access request from {email}"),
            body: format!(
                "Email: {email}\nReason: {reason}\nRequested at: {requested_at}\nReview: {}/dashboard/users\n",
                config.server.public_url.trim_end_matches('/')
            ),
        };
        tokio::spawn(async move {
            if let Err(error) = mailer.send(message).await {
                tracing::warn!(%error, "could not send access-request notification");
            }
        });
    }

    let mut view = template(viewer);
    view.submitted = true;
    Ok(Html(view).into_response())
}

fn validate(email: &str, reason: &str) -> Option<&'static str> {
    let valid_email = email.chars().count() <= 254
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    if !valid_email {
        return Some("Enter a valid email address.");
    }
    if reason.chars().count() > 2000 {
        return Some("Reason or comment must be 2,000 characters or fewer.");
    }
    None
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use sqlx::Row as _;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::db::Db;
    use crate::server::{AppState, router};

    async fn app() -> (tempfile::TempDir, Db, axum::Router) {
        app_with_config(Config::default()).await
    }

    async fn app_with_config(config: Config) -> (tempfile::TempDir, Db, axum::Router) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let app = router(AppState::new(db.clone(), config, None));
        (dir, db, app)
    }

    async fn get(app: &axum::Router) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/request-access")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        (status, body)
    }

    async fn post(app: &axum::Router, body: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/request-access")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.30")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn get_explains_reader_access() {
        let (_dir, _db, app) = app().await;
        let (status, body) = get(&app).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("every article's full text"), "{body}");
        assert!(body.contains("EPUB and XTC editions"), "{body}");
        assert!(
            body.contains("no access to ratings or admin tools"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn valid_request_is_stored_and_confirmed() {
        let (_dir, db, app) = app().await;
        let (status, body) = post(
            &app,
            "email=reader%40example.com&reason=I+love+the+paper&website=",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("requests are reviewed by hand"), "{body}");
        let row = sqlx::query("SELECT email, reason, status FROM account_requests")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("email"), "reader@example.com");
        assert_eq!(
            row.get::<Option<String>, _>("reason").as_deref(),
            Some("I love the paper")
        );
        assert_eq!(row.get::<String, _>("status"), "open");
    }

    #[tokio::test]
    async fn stored_request_queues_an_operator_notification() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let mut config = Config::default();
        config.server.public_url = "https://daily.example/".into();
        config.mail.notify_to = Some("Operator <operator@example.com>".into());
        let (mailer, messages) = crate::mail::Mailer::recording();
        let mut state = AppState::new(db, config, None);
        state.mailer = Some(mailer);
        let app = router(state);

        let (status, _) = post(&app, "email=reader%40example.com&reason=&website=").await;
        assert_eq!(status, StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !messages
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("notification task did not run");

        let messages = messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, "Operator <operator@example.com>");
        assert_eq!(
            messages[0].subject,
            "Access request from reader@example.com"
        );
        assert!(messages[0].body.contains("Email: reader@example.com"));
        assert!(messages[0].body.contains("Reason: (no reason given)"));
        assert!(messages[0].body.contains("Requested at: "));
        assert!(
            messages[0]
                .body
                .contains("Review: https://daily.example/dashboard/users")
        );
    }

    #[tokio::test]
    async fn request_access_post_is_rate_limited_per_ip() {
        let mut config = Config::default();
        config.server.login_attempts = 3;
        let (_dir, _db, app) = app_with_config(config).await;

        for _ in 0..3 {
            let (status, _) = post(&app, "email=reader%40example.com&reason=&website=").await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, _) = post(&app, "email=reader%40example.com&reason=&website=").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn honeypot_pretends_success_without_storing() {
        let (_dir, db, app) = app().await;
        let (status, body) =
            post(&app, "email=bot%40example.com&reason=spam&website=bot-site").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("requests are reviewed by hand"), "{body}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_requests")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn invalid_email_rerenders_with_an_error_without_storing() {
        let (_dir, db, app) = app().await;
        let (status, body) = post(&app, "email=not-an-email&reason=hello&website=").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Enter a valid email address."), "{body}");
        assert!(body.contains("value=\"not-an-email\""), "{body}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_requests")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn repeated_email_updates_the_open_request_case_insensitively() {
        let (_dir, db, app) = app().await;
        post(&app, "email=Reader%40Example.com&reason=first&website=").await;
        post(&app, "email=reader%40example.COM&reason=updated&website=").await;

        let rows = sqlx::query("SELECT email, reason FROM account_requests")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("email"), "reader@example.COM");
        assert_eq!(
            rows[0].get::<Option<String>, _>("reason").as_deref(),
            Some("updated")
        );
    }
}
