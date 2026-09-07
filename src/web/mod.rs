pub mod access;
pub mod dashboard;
pub mod issue;
pub mod public;
pub mod rate;
pub mod session;
pub mod timing;
pub mod users;

use std::fmt;
use std::sync::{LazyLock, Mutex};
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
use axum_login::tower_sessions::Session;

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
    /// `ExecMainStartTimestamp`, as systemd prints it; empty when never run.
    pub started: Option<String>,
    /// `ExecMainExitTimestamp`, as systemd prints it.
    pub exited: Option<String>,
}

impl UnitStatus {
    /// The unit ran and stopped with a failure result (`exit-code`, `failed`,
    /// `signal`, `timeout`, …); a never-started unit reports `success`.
    pub fn exited_unsuccessfully(&self) -> bool {
        matches!(self.active_state.as_str(), "inactive" | "failed")
            && !self.result.is_empty()
            && self.result != "success"
    }
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

/// In-memory runner for router tests: records every call and answers with
/// scripted results (`start` succeeds, `status` is inactive/success and `log`
/// is empty unless told otherwise).
#[derive(Debug, Default)]
pub struct MockRunner {
    calls: Mutex<Vec<String>>,
    start_error: Mutex<Option<String>>,
    statuses: Mutex<std::collections::HashMap<String, UnitStatus>>,
    log_text: Mutex<String>,
}

impl MockRunner {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("mock runner lock").clone()
    }

    /// Make every `start` fail with `message` (`None` restores success).
    pub fn fail_starts(&self, message: Option<&str>) {
        *self.start_error.lock().expect("mock runner lock") = message.map(str::to_string);
    }

    /// Script the `status` answer for one unit.
    pub fn set_status(&self, unit: &str, status: UnitStatus) {
        self.statuses
            .lock()
            .expect("mock runner lock")
            .insert(unit.to_string(), status);
    }

    /// Script the journal text every `log` call returns.
    pub fn set_log(&self, text: &str) {
        *self.log_text.lock().expect("mock runner lock") = text.to_string();
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("mock runner lock").push(call);
    }
}

#[async_trait]
impl JobRunner for MockRunner {
    async fn start(&self, unit: &str) -> Result<(), String> {
        self.record(format!("start {unit}"));
        match self.start_error.lock().expect("mock runner lock").clone() {
            Some(message) => Err(message),
            None => Ok(()),
        }
    }

    async fn status(&self, unit: &str) -> Result<UnitStatus, String> {
        self.record(format!("status {unit}"));
        Ok(self
            .statuses
            .lock()
            .expect("mock runner lock")
            .get(unit)
            .cloned()
            .unwrap_or_else(|| UnitStatus {
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                result: "success".into(),
                ..UnitStatus::default()
            }))
    }

    async fn log(&self, unit: &str, lines: usize) -> Result<String, String> {
        self.record(format!("log {unit} {lines}"));
        Ok(self.log_text.lock().expect("mock runner lock").clone())
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

impl WebState {
    /// Config reload on mtime (dashboard plan §4.2): when `config_path`'s
    /// modification time differs from the cached one, re-run `Config::load`
    /// and swap the live config. Returns `Ok(true)` when a reload happened,
    /// `Ok(false)` when nothing changed or no file is configured, and the
    /// load error when the file on disk no longer loads — the previous
    /// config stays live and the cached mtime is left alone so the next call
    /// tries again. Called by the settings page and by job starts.
    pub fn reload_if_changed(
        state: &crate::server::AppState,
    ) -> Result<bool, crate::config::ConfigError> {
        let Some(path) = state.config_path.as_deref() else {
            return Ok(false);
        };
        let mtime = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();
        let mut cached = state
            .web
            .config_mtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *cached == mtime {
            return Ok(false);
        }
        let config = crate::config::Config::load(Some(path))?;
        match state.config.write() {
            Ok(mut live) => *live = std::sync::Arc::new(config),
            Err(poisoned) => *poisoned.into_inner() = std::sync::Arc::new(config),
        }
        *cached = mtime;
        tracing::info!(path = %path.display(), "reloaded configuration from disk");
        Ok(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flash {
    pub kind: String,
    pub text: String,
}

/// The site-wide `<meta name="description">`, used by every page that does not
/// set one of its own. Search engines truncate around 160 characters.
pub const DEFAULT_DESCRIPTION: &str = concat!(
    "A daily newspaper of the web: articles hand-picked from one reader's feeds, ",
    "published every morning as an EPUB and readable here."
);

#[derive(Debug, Clone)]
pub struct Page {
    pub title: String,
    /// The `<meta name="description">` for this page; `DEFAULT_DESCRIPTION`
    /// unless a handler overrides it with [`Page::with_description`].
    pub description: String,
    pub viewer: Option<Viewer>,
    pub flash: Option<Flash>,
    pub active_nav: String,
    pub version: &'static str,
    /// Cache-busting token for every `/static/*` URL the site references: a
    /// content hash, so any asset change reaches browsers and edge caches that
    /// hold the previous build (a `?v=`-carrying `/static/*` URL is served with
    /// an immutable one-year `max-age`).
    pub asset_version: &'static str,
}

const NEWSREADER: &[u8] = include_bytes!("static/fonts/Newsreader.woff2");
const NEWSREADER_ITALIC: &[u8] = include_bytes!("static/fonts/Newsreader-italic.woff2");

/// The stylesheet with both Newsreader faces pointed at versioned URLs.
///
/// The faces used to be inlined here as `data:` URIs on the theory that a font
/// fetched by URL lands after first paint and flashes. That theory was wrong:
/// the flash of unstyled content came from script ordering (a parser-blocking
/// `theme.js` ahead of the stylesheet let Gecko paint before the sheet applied,
/// Bugzilla 1459305), and moving the script after the `<link>` fixed it. What
/// the inlining did cost was real: it tripled the render-blocking stylesheet to
/// ~305 KB and pushed first paint out by more than a second on mobile.
///
/// So the URLs stay URLs, carrying `?v=<ASSET_VERSION>` so the immutable
/// one-year `max-age` on a versioned `/static/*` URL is safe across deploys.
/// The fonts are part of the `ASSET_VERSION` hash, so a new face mints a new
/// URL. Neither face is preloaded — that only takes bandwidth from this sheet,
/// which is what first paint actually waits on. Instead both use
/// `font-display: swap` behind metric-matched local fallbacks, so first paint
/// is immediate and the swap shifts nothing.
pub static APP_CSS: LazyLock<String> = LazyLock::new(|| {
    let version = ASSET_VERSION.as_str();
    include_str!("static/app.css")
        .replace(
            "url(/static/Newsreader.woff2)",
            &format!("url(/static/Newsreader.woff2?v={version})"),
        )
        .replace(
            "url(/static/Newsreader-italic.woff2)",
            &format!("url(/static/Newsreader-italic.woff2?v={version})"),
        )
});

/// First 12 hex digits of the SHA-256 over *every* embedded static asset.
///
/// Every file listed here must also be referenced with `?v={ASSET_VERSION}`,
/// and every file referenced with `?v=` must be hashed here — the two halves
/// are what make the immutable one-year `max-age` safe. The favicon and the
/// speculation rules are in the hash for exactly that reason: they used to be
/// referenced by bare URL while still being served `immutable`, so editing
/// either one could never have reached a browser (or the CDN) again.
pub static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = Sha256::new();
    hasher.update(include_str!("static/app.css"));
    hasher.update(NEWSREADER);
    hasher.update(NEWSREADER_ITALIC);
    hasher.update(include_str!("static/app.js"));
    hasher.update(include_str!("static/theme.js"));
    hasher.update(include_str!("static/favicon.svg"));
    hasher.update(include_str!("static/speculation.json"));
    hex::encode(hasher.finalize())[..12].to_string()
});

/// The `Speculation-Rules` header value: a versioned URL, so a change to
/// `speculation.json` is picked up rather than pinned behind the immutable
/// one-year `max-age` on `/static/*`. Built once, like [`APP_CSS`], because it
/// interpolates [`ASSET_VERSION`] and so cannot be a `from_static`.
static SPECULATION_RULES: LazyLock<HeaderValue> = LazyLock::new(|| {
    HeaderValue::from_str(&format!(
        "\"/static/speculation.json?v={}\"",
        ASSET_VERSION.as_str()
    ))
    .expect("the asset version is hex, so the header value is valid")
});

impl Page {
    pub fn new(title: impl Into<String>, viewer: Option<Viewer>, active_nav: &str) -> Self {
        Self {
            title: title.into(),
            description: DEFAULT_DESCRIPTION.to_string(),
            viewer,
            flash: None,
            active_nav: active_nav.to_string(),
            version: crate::VERSION,
            asset_version: ASSET_VERSION.as_str(),
        }
    }

    /// Replace the site-wide description with one written for this page.
    #[must_use]
    pub fn with_description(mut self, text: impl Into<String>) -> Self {
        self.description = text.into();
        self
    }

    pub fn is_admin(&self) -> bool {
        self.viewer
            .as_ref()
            .is_some_and(|viewer| viewer.role == users::Role::Admin)
    }

    pub fn is_dashboard(&self) -> bool {
        self.is_admin()
            && matches!(
                self.active_nav.as_str(),
                "dashboard"
                    | "runs"
                    | "articles"
                    | "ratings"
                    | "profile"
                    | "stats"
                    | "jobs"
                    | "settings"
                    | "users"
            )
    }
}

/// Consume the one-shot flash message for this page, if there is one.
///
/// Reads before removing: `Session::remove` marks the session modified even
/// when the key is absent, and a modified session is written back to SQLite
/// at the end of the request, so the naive version cost every signed-in page
/// a write. Because a session is only saved when modified, its inactivity
/// expiry only slides when something changes it; [`touch_session`] keeps the
/// `server.session_days` window sliding by writing at most once a day.
pub async fn take_flash(session: &Session) -> Result<Option<Flash>, WebError> {
    let flash = session
        .get::<Flash>(FLASH_KEY)
        .await
        .map_err(session_error)?;
    if flash.is_some() {
        session
            .remove_value(FLASH_KEY)
            .await
            .map_err(session_error)?;
        return Ok(flash);
    }
    touch_session(session).await?;
    Ok(None)
}

const FLASH_KEY: &str = "flash";
/// Session key holding the unix time of the last expiry-extending write.
const TOUCHED_KEY: &str = "touched_at";
/// How often a signed-in session is written just to slide its expiry.
const TOUCH_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// Slide a signed-in session's inactivity expiry, at most once a day.
///
/// Only a session that already carries a login is touched. An anonymous
/// request must never create a session: the cookie would follow that reader
/// to every public page and take them out of the shared cache.
async fn touch_session(session: &Session) -> Result<(), WebError> {
    let signed_in = session
        .get_value(session::AUTH_DATA_KEY)
        .await
        .map_err(session_error)?
        .is_some();
    if !signed_in {
        return Ok(());
    }
    let now = Timestamp::now().as_second();
    let last = session
        .get::<i64>(TOUCHED_KEY)
        .await
        .map_err(session_error)?;
    if last.is_none_or(|last| now - last >= TOUCH_INTERVAL_SECS) {
        session
            .insert(TOUCHED_KEY, now)
            .await
            .map_err(session_error)?;
    }
    Ok(())
}

fn session_error(error: axum_login::tower_sessions::session::Error) -> WebError {
    WebError::Internal(error.into())
}

/// `Server-Timing` on every response, so the browser's DevTools (or
/// `curl -sI`) can split origin work from the network and then split the
/// origin work up again. See [`timing`] for the metrics and how they are
/// collected.
pub use self::timing::{route_boundary, server_timing};

/// Refuse TLS 1.3 0-RTT data for anything but a safe method (RFC 8470 §5.2).
///
/// With `ssl_early_data on`, nginx forwards `Early-Data: 1` for a request the
/// browser sent inside the handshake. A replayed early-data `GET` is
/// harmless; a replayed `POST` (a rating, a login attempt, a job start) is
/// not, so those get 425 and the browser resends after the handshake.
pub async fn reject_early_data(request: Request, next: Next) -> Response {
    let early = request
        .headers()
        .get("early-data")
        .is_some_and(|value| value == "1");
    if early && !request.method().is_safe() {
        return (
            StatusCode::TOO_EARLY,
            [(header::CACHE_CONTROL, "no-store")],
            "retry after the TLS handshake completes",
        )
            .into_response();
    }
    next.run(request).await
}

pub struct Html<T: Template>(pub T);

impl<T: Template> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        let started = std::time::Instant::now();
        let rendered = self.0.render();
        timing::record_render(started.elapsed());
        match rendered {
            Ok(body) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                body,
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "rendering web template failed");
                error_page_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Server error",
                    "The request could not be completed.",
                )
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
    /// BookOrbit has not indexed the requested issue yet.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
    /// A BookOrbit OPDS request failed.
    #[error("bad gateway: {0}")]
    BadGateway(String),
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

fn error_page_response(status: StatusCode, heading: &str, message: &str) -> Response {
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
            Self::ServiceUnavailable(ref message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Not indexed yet",
                message.as_str(),
            ),
            Self::BadGateway(ref message) => {
                (StatusCode::BAD_GATEWAY, "BookOrbit error", message.as_str())
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
        error_page_response(status, heading, message)
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
            "default-src 'self'; img-src * data:; font-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; form-action 'self'",
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
    // Fail closed. A response that names no policy of its own — `/login`,
    // `/account`, a redirect, an error page, or whatever route is added next —
    // is uncacheable, because a CDN with a cache-everything rule would
    // otherwise apply its own default TTL (Cloudflare: two hours on a 200) to
    // a page that may well be personalised. Handlers that mean to be cached
    // say so explicitly, and this never overrides them (§3.12).
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"))
    {
        headers.append(header::VARY, HeaderValue::from_static("Cookie"));
        headers.insert(
            header::HeaderName::from_static("speculation-rules"),
            SPECULATION_RULES.clone(),
        );
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
            post(session::login).route_layer(GovernorLayer::new(governor.clone())),
        );
    let access_routes = axum::Router::new()
        .route("/request-access", get(access::page))
        .route(
            "/request-access",
            post(access::submit).route_layer(GovernorLayer::new(governor)),
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
        ))
        .route_layer(from_fn(session::require_password_change));
    let full_issues = axum::Router::new()
        .route("/issues/{date}/articles/{article_id}", get(issue::article))
        .route("/issues/{date}/world", get(issue::world))
        .route("/issues/{date}/behind", get(issue::behind))
        .route("/issues/{date}/read", get(issue::read))
        .route_layer(login_required!(
            session::Backend,
            login_url = "/login",
            redirect_field = "next"
        ))
        .route_layer(from_fn(session::require_password_change))
        .route_layer(from_fn(map_forbidden));
    let dashboard = axum::Router::new()
        .merge(dashboard::router())
        .route("/rate", post(rate::post))
        .route_layer(permission_required!(
            session::Backend,
            login_url = "/login",
            redirect_field = "next",
            users::Role::Admin
        ))
        .route_layer(from_fn(session::require_password_change))
        .route_layer(from_fn(map_forbidden));

    axum::Router::new()
        .route("/", get(public::latest))
        .route("/issues", get(public::archive))
        .route("/issues/{date}", get(public::show_issue))
        .route("/feed.xml", get(public::feed))
        .route("/robots.txt", get(public::robots))
        .route("/static/{file}", get(static_asset))
        .merge(access_routes)
        .merge(login)
        .merge(account)
        .merge(full_issues)
        .merge(dashboard)
        .fallback(|| async { WebError::NotFound })
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

/// The `?v=` cache-buster on a `/static/*` URL, when the reference carries one.
#[derive(Debug, Deserialize)]
struct AssetQuery {
    #[serde(default)]
    v: Option<String>,
}

/// A versioned URL names one immutable build, so it may be held for a year.
const VERSIONED_CACHE: &str = "public, max-age=31536000, immutable";

/// A bare `/static/…` URL is not a promise about its content, so it gets an
/// hour and revalidates against the ETag. Someone else's link or an old
/// bookmark must not pin a stale asset for a year.
const UNVERSIONED_CACHE: &str = "public, max-age=3600";

async fn static_asset(
    axum::extract::Path(file): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AssetQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let asset: (&str, &'static [u8]) = match file.as_str() {
        "app.css" => ("text/css; charset=utf-8", APP_CSS.as_bytes()),
        "app.js" => (
            "application/javascript; charset=utf-8",
            include_str!("static/app.js").as_bytes(),
        ),
        "theme.js" => (
            "application/javascript; charset=utf-8",
            include_str!("static/theme.js").as_bytes(),
        ),
        "speculation.json" => (
            "application/speculationrules+json",
            include_str!("static/speculation.json").as_bytes(),
        ),
        "favicon.svg" => (
            "image/svg+xml",
            include_str!("static/favicon.svg").as_bytes(),
        ),
        "Newsreader.woff2" => ("font/woff2", NEWSREADER),
        "Newsreader-italic.woff2" => ("font/woff2", NEWSREADER_ITALIC),
        _ => return WebError::NotFound.into_response(),
    };
    let cache_control = if query.v.is_some() {
        VERSIONED_CACHE
    } else {
        UNVERSIONED_CACHE
    };
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(asset.1)));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache_control.to_string()),
            ],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.0.to_string()),
            (header::CACHE_CONTROL, cache_control.to_string()),
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

    /// The `db` metric only fills in when [`timing::sqlx_timing_layer`] is
    /// installed, and it has to be installed *globally*: sqlx times statements
    /// on a worker thread, which sees no thread-local default. Global means
    /// once per test binary, hence the `Once`; nothing is written anywhere, so
    /// the tests that do not care never notice it.
    fn install_timing_layer() {
        use tracing_subscriber::layer::SubscriberExt as _;
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            let subscriber = tracing_subscriber::registry().with(timing::sqlx_timing_layer());
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
    }

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
    async fn take_flash_reads_without_dirtying_an_empty_session() {
        let (_dir, state) = test_state(Config::default()).await;
        let store = std::sync::Arc::new(session::SqliteSessionStore::new(state.db.pool().clone()));
        let session = Session::new(None, store, None);
        assert_eq!(take_flash(&session).await.unwrap(), None);
        assert!(!session.is_modified());
        assert!(session.is_empty().await);

        let flash = Flash {
            kind: "ok".into(),
            text: "saved".into(),
        };
        session.insert(FLASH_KEY, &flash).await.unwrap();
        assert_eq!(take_flash(&session).await.unwrap(), Some(flash));
        assert_eq!(take_flash(&session).await.unwrap(), None);
    }

    #[tokio::test]
    async fn signed_in_pages_touch_the_session_once_a_day_not_every_request() {
        let mut config = Config::default();
        config.server.public_url = "https://daily.example".into();
        let (_dir, state) = test_state(config).await;
        // Only the dashboard and the signed-in issue pages consume flashes, so
        // an admin exercises the path without an issue in the database.
        users::add(&state.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let app = router(state.clone());
        let cookie = login_cookie(&app, "admin", "correct horse battery").await;
        let stamp = || async {
            sqlx::query_scalar::<_, String>("SELECT updated_at || ' ' || data FROM sessions")
                .fetch_one(state.db.pool())
                .await
                .unwrap()
        };
        let get = |uri: &str| {
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap()
        };
        // The first page after login writes the daily touch stamp…
        let response = app
            .clone()
            .oneshot(get("/dashboard/articles"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let first = stamp().await;
        assert!(first.contains(TOUCHED_KEY), "{first}");
        // …and the pages after it leave the row alone.
        for uri in [
            "/dashboard/articles",
            "/dashboard/ratings",
            "/dashboard/stats",
            "/",
            "/account",
        ] {
            let response = app.clone().oneshot(get(uri)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
        assert_eq!(stamp().await, first);
    }

    #[tokio::test]
    async fn server_timing_header_and_early_data_guard() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let timing = response
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(timing.starts_with("app;dur="), "{timing}");
        // `/healthz` is routed but touches neither SQLite nor a template.
        assert!(timing.contains("sess;dur="), "{timing}");
        assert!(!timing.contains("db;dur="), "{timing}");
        assert!(!timing.contains("tpl;dur="), "{timing}");

        // A safe method may arrive as 0-RTT data; a POST may not.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("early-data", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut request = post("/login", "username=a&password=b", "192.0.2.1");
        request
            .headers_mut()
            .insert("early-data", HeaderValue::from_static("1"));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_EARLY);
    }

    /// The `db` metric crosses a thread boundary to get here: sqlx runs the
    /// statement on the connection's worker thread and only reports the
    /// elapsed time there, inside the span this request handed it. This is the
    /// test that the hand-off actually holds together.
    #[tokio::test]
    async fn server_timing_breaks_the_request_into_session_db_and_template() {
        install_timing_layer();
        let (_dir, state) = test_state(Config::default()).await;
        users::add(&state.db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let app = router(state);
        let cookie = login_cookie(&app, "admin", "correct horse battery").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/articles")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let timing = response
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Every metric is present, and each is a number the spec allows.
        assert!(timing.starts_with("app;dur="), "{timing}");
        let mut durations = std::collections::HashMap::new();
        for metric in timing.split(", ") {
            let (name, rest) = metric.split_once(";dur=").expect(&timing);
            let number = rest.split(';').next().unwrap();
            durations.insert(name, number.parse::<f64>().expect(&timing));
        }
        for name in ["app", "sess", "db", "tpl"] {
            assert!(durations.contains_key(name), "{name} missing from {timing}");
        }

        // A dashboard page loads the session and renders, so each of those is
        // inside the whole rather than beside it. `db` is left out: it is a
        // sum of statement times, which a handler running queries concurrently
        // could legitimately push past the wall clock.
        let app_dur = durations["app"];
        for name in ["sess", "tpl"] {
            assert!(durations[name] <= app_dur, "{name} exceeds app in {timing}");
        }
        // `sess` is the session load and save; `db` includes those statements
        // and the page's own, so neither is a subset of the other, but both
        // had to have measured something.
        assert!(durations["db"] > 0.0, "{timing}");

        // The statement count rides along in `desc`, which is where DevTools
        // shows it. A dashboard page runs several: session, user, articles.
        let db = timing
            .split(", ")
            .find(|metric| metric.starts_with("db;"))
            .expect(&timing);
        let count: u32 = db
            .split("desc=\"")
            .nth(1)
            .and_then(|desc| desc.split_whitespace().next())
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("no statement count in {db}"));
        assert!(count > 1, "{db}");
        assert!(db.ends_with(" queries\""), "{db}");
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
        assert_eq!(
            anonymous.headers().get("speculation-rules").unwrap(),
            &format!("\"/static/speculation.json?v={}\"", ASSET_VERSION.as_str())
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
        let settings = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dashboard/settings")
                    .header(header::COOKIE, &admin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(settings.status(), StatusCode::OK);
        let settings = response_text(settings).await;
        assert!(settings.contains("<span>Dashboard</span>"), "{settings}");
        assert!(!settings.contains("Morning edition"), "{settings}");
        assert!(
            settings.contains(
                "border-accent text-ink\" href=\"/dashboard/settings\" aria-current=\"page\""
            ),
            "{settings}"
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
        assert_eq!(allowed_rate.status(), StatusCode::BAD_REQUEST);
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
        let shared = app
            .clone()
            .oneshot(post(
                "/request-access",
                "email=reader%40example.com&reason=&website=",
                "192.0.2.20",
            ))
            .await
            .unwrap();
        assert_eq!(shared.status(), StatusCode::TOO_MANY_REQUESTS);
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

    #[test]
    fn asset_urls_carry_a_content_hash_not_the_crate_version() {
        let page = Page::new("t", None, "latest");
        assert_eq!(page.asset_version.len(), 12);
        assert!(page.asset_version.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(page.asset_version, crate::VERSION);
        let html = ErrorTemplate {
            page,
            heading: "h".into(),
            message: "m".into(),
        }
        .render()
        .unwrap();
        let expected = format!("/static/app.css?v={}", ASSET_VERSION.as_str());
        assert!(html.contains(&expected), "{html}");
        assert!(!html.contains(&format!("/static/app.css?v={}", crate::VERSION)));
        // Nothing is preloaded. A preload of the 132 KB regular face shares
        // bandwidth with the 12 KB render-blocking stylesheet, which is what
        // first paint actually waits on: it pushed simulated FCP from ~1.0 s to
        // ~1.8 s on Lighthouse mobile, for a face the metric-matched fallbacks
        // already stand in for.
        assert!(!html.contains("rel=\"preload\""), "{html}");
        // The italic face is never referenced from the HTML either.
        assert!(!html.contains("Newsreader-italic.woff2"), "{html}");
        // `defer` keeps app.js out of Lighthouse's render-blocking list; it has
        // no readyState or DOMContentLoaded dependence, so deferring is safe.
        let app_js = format!(
            "<script defer src=\"/static/app.js?v={}\"></script>",
            ASSET_VERSION.as_str()
        );
        assert!(html.contains(&app_js), "{html}");
    }

    #[test]
    fn pages_render_a_meta_description_and_escape_it() {
        let render = |page: Page| {
            ErrorTemplate {
                page,
                heading: "h".into(),
                message: "m".into(),
            }
            .render()
            .unwrap()
        };

        // Every page carries a description; the default one when none is set.
        assert!(DEFAULT_DESCRIPTION.len() <= 160, "{DEFAULT_DESCRIPTION}");
        let html = render(Page::new("t", None, "latest"));
        assert!(
            html.contains(
                "<meta name=\"description\" content=\"A daily newspaper of the web: articles \
                 hand-picked from one reader&#39;s feeds, published every morning as an EPUB and \
                 readable here.\">"
            ),
            "{html}"
        );

        // A page-specific one replaces it, HTML-escaped into the attribute.
        let page = Page::new("t", None, "latest")
            .with_description("Issue \"No. 3\" & <b>4</b> for O'Donnell");
        assert_eq!(page.description, "Issue \"No. 3\" & <b>4</b> for O'Donnell");
        let html = render(page);
        assert!(
            html.contains(
                "<meta name=\"description\" content=\"Issue &#34;No. 3&#34; &#38; \
                 &#60;b&#62;4&#60;/b&#62; for O&#39;Donnell\">"
            ),
            "{html}"
        );
        assert!(!html.contains("<b>4</b>"), "{html}");
        assert!(!html.contains(DEFAULT_DESCRIPTION), "{html}");
    }

    #[test]
    fn stylesheet_points_both_newsreader_faces_at_versioned_urls() {
        let css = APP_CSS.as_str();
        let version = ASSET_VERSION.as_str();
        assert!(
            css.contains(&format!("url(/static/Newsreader.woff2?v={version})")),
            "regular face missing from stylesheet"
        );
        assert!(
            css.contains(&format!("url(/static/Newsreader-italic.woff2?v={version})")),
            "italic face missing from stylesheet"
        );
        // Unversioned references would be cached forever under a stale URL, and
        // inlined faces would put ~280 KB back into the render-blocking sheet.
        assert!(!css.contains("url(/static/Newsreader.woff2)"));
        assert!(!css.contains("url(/static/Newsreader-italic.woff2)"));
        assert!(!css.contains("data:font"));
        // `swap` paints immediately in the metric-matched local fallback.
        assert_eq!(css.matches("font-display:swap").count(), 2);
        assert!(!css.contains("font-display:block"));
    }

    #[tokio::test]
    async fn static_assets_use_content_hash_etags() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let versioned = format!("/static/app.css?v={}", ASSET_VERSION.as_str());
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(versioned.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        let etag = first.headers().get(header::ETAG).unwrap().clone();
        let cached = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(versioned.as_str())
                    .header(header::IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            cached.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );

        // Without `?v=` the URL is not a promise about its content: an hour,
        // revalidated against the same ETag, never a pinned year.
        let bare = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bare.status(), StatusCode::OK);
        assert_eq!(
            bare.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=3600"
        );
        assert_eq!(bare.headers().get(header::ETAG).unwrap(), &etag);
        let bare_cached = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/static/app.css")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bare_cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            bare_cached.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=3600"
        );

        let rules = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/static/speculation.json?v={}",
                        ASSET_VERSION.as_str()
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rules.status(), StatusCode::OK);
        assert_eq!(
            rules.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/speculationrules+json"
        );
        assert_eq!(
            rules.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
    }

    /// Everything the layout and the headers point at must carry `?v=`: an
    /// unversioned reference to an immutably cached asset can never be updated.
    #[tokio::test]
    async fn every_referenced_static_url_is_versioned() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let version = ASSET_VERSION.as_str();
        assert_eq!(
            response.headers().get("speculation-rules").unwrap(),
            &format!("\"/static/speculation.json?v={version}\"")
        );
        let html = response_text(response).await;
        for asset in ["favicon.svg", "app.css", "theme.js"] {
            assert!(
                html.contains(&format!("/static/{asset}?v={version}")),
                "{asset} is referenced without ?v= in {html}"
            );
            assert!(
                !html.contains(&format!("\"/static/{asset}\"")),
                "{asset} still has a bare reference in {html}"
            );
        }
        // The fonts are reached through the stylesheet, not the layout.
        assert!(APP_CSS.contains(&format!("url(/static/Newsreader.woff2?v={version})")));
    }

    /// A `\\?` inside an `href_matches` *string* never means what it looks
    /// like. A URL pattern string is split into components before escapes are
    /// resolved, so the `?` still ends the pathname: `"/*\\?*"` parses as
    /// pathname `/*` with search `*`, which matches every same-origin URL.
    /// Buried in a `not`, that silently excluded everything and no link was
    /// ever speculated. Filter on the query with the object form instead —
    /// `{"pathname": "/*", "search": "(.+)"}` — whose components are separate
    /// by construction.
    #[test]
    fn speculation_rules_never_match_the_query_string_from_a_pattern_string() {
        let rules: serde_json::Value =
            serde_json::from_str(include_str!("static/speculation.json"))
                .expect("speculation.json is valid JSON");

        fn walk(value: &serde_json::Value, strings: &mut Vec<String>, objects: &mut usize) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        if key == "href_matches" {
                            let patterns = match child {
                                serde_json::Value::Array(items) => items.clone(),
                                other => vec![other.clone()],
                            };
                            for pattern in patterns {
                                match pattern {
                                    serde_json::Value::String(text) => strings.push(text),
                                    serde_json::Value::Object(components) => {
                                        assert!(
                                            !components.contains_key("search")
                                                || components.contains_key("pathname"),
                                            "a component pattern naming `search` must also name \
                                             `pathname`, or the pathname is inherited from the \
                                             document URL and the pattern matches nothing"
                                        );
                                        *objects += 1;
                                    }
                                    other => panic!(
                                        "href_matches takes a string or an object, not {other}"
                                    ),
                                }
                            }
                        }
                        walk(child, strings, objects);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, strings, objects);
                    }
                }
                _ => {}
            }
        }

        let mut strings = Vec::new();
        let mut objects = 0;
        walk(&rules, &mut strings, &mut objects);

        assert!(
            !strings.is_empty(),
            "the rules matched no href_matches at all"
        );
        for pattern in &strings {
            assert!(
                !pattern.contains('?'),
                "`{pattern}` reaches for the query from a pattern string; use the object form"
            );
        }
        assert!(
            objects > 0,
            "no component pattern is left to exclude query URLs"
        );
    }

    /// A response that names no policy of its own must not be cacheable: a CDN
    /// cache-everything rule would otherwise give it the CDN's default TTL.
    #[tokio::test]
    async fn responses_without_a_policy_default_to_no_store() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        for uri in ["/login", "/healthz", "/no-such-page"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-store",
                "{uri}"
            );
        }
    }

    /// Cookie- and Basic-auth-gated routes must be uncacheable everywhere, on
    /// every status code, whatever the CDN is told to do.
    #[tokio::test]
    async fn download_and_opds_routes_are_private_no_store() {
        let mut config = Config::default();
        config.publish.epub_dir = std::path::PathBuf::from("/nonexistent/epub");
        config.publish.xtc_dir = std::path::PathBuf::from("/nonexistent/xtc");
        let (_dir, state) = test_state(config).await;
        let app = router(state);
        for uri in [
            "/opds",
            "/opds/",
            crate::publish::OPDS_PATH,
            "/files/epub/missing.epub",
            "/files/xtc/missing.xtch",
            "/files/epub/..%2Fescape",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "private, no-store",
                "{uri} answered {}",
                response.status()
            );
        }

        // The Basic auth challenge is a response too.
        let mut config = Config::default();
        config.server.basic_auth_user = Some("daily".into());
        config.server.basic_auth_pass = Some("hunter2".into());
        let (_dir, state) = test_state(config).await;
        let app = router(state);
        let challenge = app
            .oneshot(Request::builder().uri("/opds").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            challenge.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
    }

    #[tokio::test]
    async fn not_found_and_server_error_pages_use_the_site_layout() {
        let (_dir, state) = test_state(Config::default()).await;
        let app = router(state);
        let missing = app
            .oneshot(
                Request::builder()
                    .uri("/no-such-page")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let missing = response_text(missing).await;
        assert!(missing.contains("<!doctype html>"), "{missing}");
        assert!(missing.contains("The Daily EPUB"), "{missing}");
        assert!(missing.contains("That page does not exist"), "{missing}");

        let failed = WebError::Internal(anyhow::anyhow!("fixture failure")).into_response();
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let failed = response_text(failed).await;
        assert!(failed.contains("<!doctype html>"), "{failed}");
        assert!(failed.contains("The Daily EPUB"), "{failed}");
        assert!(
            failed.contains("The request could not be completed"),
            "{failed}"
        );
        assert!(!failed.contains("fixture failure"), "{failed}");
    }
}
