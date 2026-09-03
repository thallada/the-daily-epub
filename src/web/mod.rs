pub mod issue;
pub mod public;
pub mod session;
pub mod users;

use std::fmt;
use std::sync::Mutex;
use std::time::SystemTime;

use askama::Template;
use async_trait::async_trait;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use self::session::Viewer;

#[async_trait]
pub trait JobRunner: Send + Sync {
    async fn start(&self, unit: &str) -> Result<(), String>;
    async fn status(&self, unit: &str) -> Result<UnitStatus, String>;
    async fn log(&self, unit: &str, lines: usize) -> Result<String, String>;
}

#[derive(Debug, Clone, Default)]
pub struct UnitStatus {
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Default)]
pub struct DisabledRunner;

#[async_trait]
impl JobRunner for DisabledRunner {
    async fn start(&self, _unit: &str) -> Result<(), String> {
        Err("jobs are disabled".into())
    }

    async fn status(&self, _unit: &str) -> Result<UnitStatus, String> {
        Err("jobs are disabled".into())
    }

    async fn log(&self, _unit: &str, _lines: usize) -> Result<String, String> {
        Err("jobs are disabled".into())
    }
}

/// In-memory runner for router tests. Step 6 will add scripted results alongside
/// these recorded calls when the jobs pages begin invoking the runner.
#[derive(Debug, Default)]
pub struct MockRunner {
    calls: Mutex<Vec<String>>,
}

impl MockRunner {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("mock runner lock").clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("mock runner lock").push(call);
    }
}

#[async_trait]
impl JobRunner for MockRunner {
    async fn start(&self, unit: &str) -> Result<(), String> {
        self.record(format!("start {unit}"));
        Ok(())
    }

    async fn status(&self, unit: &str) -> Result<UnitStatus, String> {
        self.record(format!("status {unit}"));
        Ok(UnitStatus::default())
    }

    async fn log(&self, unit: &str, lines: usize) -> Result<String, String> {
        self.record(format!("log {unit} {lines}"));
        Ok(String::new())
    }
}

pub struct WebState {
    pub jobs: std::sync::Arc<dyn JobRunner>,
    pub started_at: Timestamp,
    pub config_mtime: Mutex<Option<SystemTime>>,
}

impl fmt::Debug for WebState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebState")
            .field("started_at", &self.started_at)
            .field("config_mtime", &self.config_mtime)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flash {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub title: String,
    pub viewer: Option<Viewer>,
    pub flash: Option<Flash>,
    pub active_nav: String,
    pub version: &'static str,
}

impl Page {
    pub fn new(title: impl Into<String>, viewer: Option<Viewer>, active_nav: &str) -> Self {
        Self {
            title: title.into(),
            viewer,
            flash: None,
            active_nav: active_nav.to_string(),
            version: crate::VERSION,
        }
    }

    pub fn is_admin(&self) -> bool {
        self.viewer
            .as_ref()
            .is_some_and(|viewer| viewer.role == users::Role::Admin)
    }
}

pub struct Html<T: Template>(pub T);

impl<T: Template> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(body) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                body,
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "rendering web template failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("authentication required")]
    Unauthenticated { next: String },
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("request origin did not match this site")]
    Csrf,
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    page: Page,
    heading: String,
    message: String,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        if let Self::Unauthenticated { next } = self {
            return axum::response::Redirect::temporary(&format!(
                "/login?next={}",
                encode_component(&next)
            ))
            .into_response();
        }
        let (status, heading, message) = match self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "Not found",
                "That page does not exist.",
            ),
            Self::Forbidden | Self::Csrf => (
                StatusCode::FORBIDDEN,
                "Forbidden",
                "You do not have permission to do that.",
            ),
            Self::BadRequest(ref message) => {
                (StatusCode::BAD_REQUEST, "Bad request", message.as_str())
            }
            Self::Db(ref error) => {
                tracing::error!(%error, "web database request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Server error",
                    "The request could not be completed.",
                )
            }
            Self::Internal(ref error) => {
                tracing::error!(%error, "web request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Server error",
                    "The request could not be completed.",
                )
            }
            Self::Unauthenticated { .. } => unreachable!(),
        };
        let rendered = ErrorTemplate {
            page: Page::new(heading, None, ""),
            heading: heading.to_string(),
            message: message.to_string(),
        }
        .render()
        .unwrap_or_else(|_| message.to_string());
        (
            status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            rendered,
        )
            .into_response()
    }
}

pub fn encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Pagination {
    pub page: u32,
    pub per_page: u32,
    pub total: i64,
}

impl Pagination {
    pub fn offset(self) -> i64 {
        i64::from(self.page.saturating_sub(1)) * i64::from(self.per_page)
    }

    pub fn pages(self) -> u32 {
        ((self.total.max(0) as u64).div_ceil(u64::from(self.per_page))) as u32
    }
}

pub fn format_time(timestamp: Timestamp, config: &crate::config::Config) -> String {
    config
        .tz()
        .map(|tz| {
            timestamp
                .to_zoned(tz)
                .strftime("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

pub async fn security_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; img-src * data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    if path.starts_with("/dashboard") {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"))
    {
        headers.append(header::VARY, HeaderValue::from_static("Cookie"));
    }
    response
}

pub fn router(config: &crate::config::Config) -> axum::Router<crate::server::AppState> {
    use axum::middleware::from_fn;
    use axum::routing::{get, post};
    use axum_login::{login_required, permission_required};
    use tower_governor::GovernorLayer;
    use tower_governor::governor::GovernorConfigBuilder;
    use tower_governor::key_extractor::SmartIpKeyExtractor;

    let seconds_per_token = (u64::from(config.server.login_window_minutes) * 60
        / u64::from(config.server.login_attempts))
    .max(1);
    let governor = std::sync::Arc::new(
        GovernorConfigBuilder::default()
            .per_second(seconds_per_token)
            .burst_size(config.server.login_attempts)
            .key_extractor(SmartIpKeyExtractor)
            .finish()
            .expect("validated non-zero login governor configuration"),
    );
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let cleanup = governor.clone();
        handle.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                cleanup.limiter().retain_recent();
            }
        });
    }

    let login = axum::Router::new()
        .route("/login", get(session::login_page))
        .route(
            "/login",
            post(session::login).route_layer(GovernorLayer::new(governor)),
        );
    let account = axum::Router::new()
        .route("/account", get(session::account))
        .route("/account/password", post(session::change_password))
        .route("/account/logout-all", post(session::logout_everywhere))
        .route("/logout", post(session::logout))
        .route_layer(login_required!(
            session::Backend,
            login_url = "/login",
            redirect_field = "next"
        ));
    let dashboard = axum::Router::new()
        .route("/dashboard", get(dashboard_stub))
        .route("/rate", post(rate_stub))
        .route_layer(permission_required!(
            session::Backend,
            login_url = "/login",
            redirect_field = "next",
            users::Role::Admin
        ))
        .route_layer(from_fn(map_forbidden));

    axum::Router::new()
        .route("/", get(public::latest))
        .route("/issues", get(public::archive))
        .route("/issues/{date}", get(public::show_issue))
        .route("/feed.xml", get(public::feed))
        .route("/robots.txt", get(public::robots))
        .route("/static/{file}", get(static_asset))
        .merge(login)
        .merge(account)
        .merge(dashboard)
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
struct OverviewTemplate {
    page: Page,
}

async fn dashboard_stub(auth: session::AuthSession) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(session::Viewer::from);
    Ok(Html(OverviewTemplate {
        page: Page::new("Overview", viewer, "dashboard"),
    })
    .into_response())
}

async fn rate_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn map_forbidden(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if response.status() == StatusCode::FORBIDDEN {
        WebError::Forbidden.into_response()
    } else {
        if response.status() == StatusCode::TEMPORARY_REDIRECT {
            *response.status_mut() = StatusCode::FOUND;
        }
        response
    }
}

async fn static_asset(
    axum::extract::Path(file): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let asset = match file.as_str() {
        "app.css" => ("text/css; charset=utf-8", include_str!("static/app.css")),
        "app.js" => (
            "application/javascript; charset=utf-8",
            include_str!("static/app.js"),
        ),
        "favicon.svg" => ("image/svg+xml", include_str!("static/favicon.svg")),
        _ => return WebError::NotFound.into_response(),
    };
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(asset.1.as_bytes())));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "public, max-age=86400".into()),
            ],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.0.to_string()),
            (header::CACHE_CONTROL, "public, max-age=86400".into()),
            (header::ETAG, etag),
        ],
        asset.1,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::server::{AppState, router};

    async fn test_state(config: Config) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        (dir, AppState::new(db, config, None))
    }

    fn post(uri: &str, body: &str, ip: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin")
            .header("x-forwarded-for", ip)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn login_cookie(app: &axum::Router, username: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(post(
                "/login",
                &format!("username={username}&password={password}&next=%2F"),
                "192.0.2.1",
            ))
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

    async fn response_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn login_cookie_account_logout_and_anonymous_pages() {
        let mut config = Config::default();
        config.server.public_url = "https://daily.example".into();
        let (_dir, state) = test_state(config).await;
        users::add(&state.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let app = router(state.clone());

        let anonymous = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(anonymous.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(
            anonymous.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
        );

        let response = app
            .clone()
            .oneshot(post(
                "/login",
                "username=admin&password=correct+horse+battery&next=%2Faccount",
                "192.0.2.3",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/account"
        );
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("daily_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(set_cookie.contains("Secure"));
        let cookie = set_cookie.split(';').next().unwrap();

        let account = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(account.status(), StatusCode::OK);
        assert!(response_text(account).await.contains("admin"));

        let logout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/logout")
                    .header(header::COOKIE, cookie)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn guards_distinguish_anonymous_users_and_admins() {
        let (_dir, state) = test_state(Config::default()).await;
        users::add(&state.db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        users::add(&state.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let app = router(state);
        let anonymous = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::FOUND);
        assert_eq!(
            anonymous.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fdashboard"
        );
        let anonymous_rate = app
            .clone()
            .oneshot(post("/rate", "", "192.0.2.20"))
            .await
            .unwrap();
        assert_eq!(anonymous_rate.status(), StatusCode::FOUND);
        assert_eq!(
            anonymous_rate.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Frate"
        );

        let reader = login_cookie(&app, "reader", "correct horse battery").await;
        let forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .header(header::COOKIE, &reader)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(response_text(forbidden).await.contains("Forbidden"));
        let forbidden_rate = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, &reader)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden_rate.status(), StatusCode::FORBIDDEN);

        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let allowed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .header(header::COOKIE, &admin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert_eq!(
            allowed.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let allowed_rate = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, admin)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed_rate.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn origin_check_rejects_cross_site_and_foreign_origins() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let cross = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header("sec-fetch-site", "cross-site")
                    .header("x-forwarded-for", "192.0.2.10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross.status(), StatusCode::FORBIDDEN);
        let foreign = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "daily.hallada.net")
                    .header("x-forwarded-proto", "https")
                    .header("x-forwarded-for", "192.0.2.11")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
        let same = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::ORIGIN, "https://daily.hallada.net")
                    .header(header::HOST, "daily.hallada.net")
                    .header("x-forwarded-proto", "https")
                    .header("x-forwarded-for", "192.0.2.12")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("username=x&password=invalid-invalid"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(same.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_throttle_is_per_ip() {
        let mut config = Config::default();
        config.server.login_attempts = 3;
        let (_dir, state) = test_state(config).await;
        let app = router(state);
        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(post(
                    "/login",
                    "username=nobody&password=invalid-invalid",
                    "192.0.2.20",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let limited = app
            .clone()
            .oneshot(post(
                "/login",
                "username=nobody&password=invalid-invalid",
                "192.0.2.20",
            ))
            .await
            .unwrap();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let other = app
            .oneshot(post(
                "/login",
                "username=nobody&password=invalid-invalid",
                "192.0.2.21",
            ))
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn disabled_users_and_password_changes_invalidate_other_sessions() {
        let (_dir, state) = test_state(Config::default()).await;
        users::add(&state.db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = router(state.clone());
        let first = login_cookie(&app, "reader", "correct horse battery").await;
        let second = login_cookie(&app, "reader", "correct horse battery").await;
        let changed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/account/password")
                    .header(header::COOKIE, &first)
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("current_password=correct+horse+battery&new_password=a+replacement+password&confirm_password=a+replacement+password"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::SEE_OTHER);
        let old_session = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .header(header::COOKIE, second)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(old_session.status().is_redirection());

        let fresh = login_cookie(&app, "reader", "a replacement password").await;
        users::set_disabled(&state.db, "reader", true)
            .await
            .unwrap();
        let disabled = app
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .header(header::COOKIE, fresh)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(disabled.status().is_redirection());
    }

    #[tokio::test]
    async fn files_accept_a_session_or_basic_auth() {
        let mut config = Config::default();
        config.server.basic_auth_user = Some("opds".into());
        config.server.basic_auth_pass = Some("hunter2".into());
        let (dir, state) = test_state(config).await;
        let epub_dir = dir.path().join("epub");
        std::fs::create_dir_all(&epub_dir).unwrap();
        std::fs::write(epub_dir.join("issue.epub"), b"epub").unwrap();
        {
            let mut live = state.config.write().unwrap();
            let mut changed = (**live).clone();
            changed.publish.epub_dir = epub_dir;
            *live = std::sync::Arc::new(changed);
        }
        users::add(&state.db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = router(state);
        let anonymous = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/files/epub/issue.epub")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert!(anonymous.headers().contains_key(header::WWW_AUTHENTICATE));
        let basic = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/files/epub/issue.epub")
                    .header(header::AUTHORIZATION, "Basic b3BkczpodW50ZXIy")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(basic.status(), StatusCode::OK);
        let cookie = login_cookie(&app, "reader", "correct horse battery").await;
        let session = app
            .oneshot(
                Request::builder()
                    .uri("/files/epub/issue.epub")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_assets_use_content_hash_etags() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=86400"
        );
        let etag = first.headers().get(header::ETAG).unwrap().clone();
        let cached = app
            .oneshot(
                Request::builder()
                    .uri("/static/app.css")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    }
}
