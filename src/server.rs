//! axum server: rating endpoints, the OPDS catalog, downloads (spec §3.9, §3.12).
//!
//! Rating links must work from an e-reader's built-in browser, so every rating
//! endpoint is a `GET` and the response is a tiny e-ink-sized HTML page.
//!
//! Routes (§3.12):
//! | route | behaviour |
//! |---|---|
//! | `GET /r/{date}/{article_id}/{vote}?t=` | verify HMAC and append a rating event |
//! | `GET /opds/daily.xml` (also `/opds`, `/opds/`) | OPDS 1.2 acquisition feed over `publish.epub_dir` |
//! | `GET /files/epub/{name}` | EPUB download — what the feed's acquisition links point at |
//! | `GET /files/xtc/{name}` | XTC artifact download, unlisted (no path traversal) |
//! | `GET /healthz` | liveness |
//! | `GET /issues.json` | the last 30 run reports, newest first |
//!
//! `/opds/*` and `/files/*` sit behind optional Basic auth (`server.basic_auth_*`)
//! and are therefore `private, no-store` on every response, 401s and 404s
//! included: the site sits behind a CDN whose cache-everything rule would
//! otherwise turn an authenticated download into a public one (§3.12).
//!
//! The EPUB article footer (§3.10) mints its three verdict links with the very same
//! [`rating_url`] this module verifies with — both re-export [`crate::auth`],
//! which pins the shared test vector (`secret = "test-secret"`, `2026-08-15`,
//! article `42`, `up` → `3b314cf7e6d8f50f`). An issue generated while
//! `server.hmac_secret` is unset carries links this server rejects with 403.

use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::AuthManagerLayerBuilder;
use axum_login::tower_sessions::{Expiry, SessionManagerLayer};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use jiff::Timestamp;
use jiff::civil::Date;
use serde::Deserialize;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::db::Db;
use crate::types::{ArticleId, RatingEvent, Vote};
use crate::web::public::{PRIVATE_CACHE, PUBLIC_CACHE};

/// Characters of the hex HMAC kept in rating links (§3.9).
pub const TOKEN_LEN: usize = crate::auth::TOKEN_LEN;
/// How many issues `GET /issues.json` returns (§3.12).
pub const ISSUES_JSON_LIMIT: i64 = 30;
/// Basic auth realm advertised for the OPDS routes (§3.11).
pub const AUTH_REALM: &str = "The Daily EPUB";
/// Content type of an OPDS 1.2 acquisition feed (§3.11).
pub const OPDS_CONTENT_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("server.hmac_secret is not configured (set DAILY_EPUB_SERVER__HMAC_SECRET)")]
    MissingSecret,
    #[error("could not bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not configure outbound mail: {0}")]
    Mail(#[source] anyhow::Error),
}

/// Shared axum state.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<RwLock<Arc<Config>>>,
    pub config_path: Option<PathBuf>,
    pub web: Arc<crate::web::WebState>,
    /// Startup-built SMTP sender. Mail configuration changes require a restart.
    pub mailer: Option<crate::mail::Mailer>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db", &self.db)
            .field("config_path", &self.config_path)
            .field("web", &self.web)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn new(db: Db, config: Config, config_path: Option<PathBuf>) -> Self {
        Self {
            db,
            config: Arc::new(RwLock::new(Arc::new(config))),
            config_path,
            web: Arc::new(crate::web::WebState {
                jobs: Arc::new(crate::web::DisabledRunner),
                started_at: Timestamp::now(),
                config_mtime: std::sync::Mutex::new(None),
            }),
            mailer: None,
        }
    }

    /// `new`, with a specific [`crate::web::JobRunner`]: `SystemdRunner` in
    /// `serve`, a `MockRunner` in router tests (dashboard plan §14.4).
    pub fn with_jobs(
        db: Db,
        config: Config,
        config_path: Option<PathBuf>,
        jobs: Arc<dyn crate::web::JobRunner>,
    ) -> Self {
        let mut state = Self::new(db, config, config_path);
        state.web = Arc::new(crate::web::WebState {
            jobs,
            started_at: Timestamp::now(),
            config_mtime: std::sync::Mutex::new(None),
        });
        state
    }

    pub fn config(&self) -> Arc<Config> {
        match self.config.read() {
            Ok(config) => Arc::clone(&config),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }
}

// ---------------------------------------------------------------------------
// Rating tokens (§3.9)
// ---------------------------------------------------------------------------

// The formula lives in [`crate::auth`] so the EPUB writer and this verifier can
// never drift apart; these re-exports keep the historical call sites intact.
pub use crate::auth::{constant_time_eq, rating_token, rating_url, verify_token};

// ---------------------------------------------------------------------------
// Router (§3.12)
// ---------------------------------------------------------------------------

/// Build the router: `/r/{date}/{article_id}/{vote}`, `/opds/daily.xml`,
/// `/files/epub/{name}`, `/files/xtc/{name}`, `/healthz`, `/issues.json`, with
/// `tower-http` tracing (§3.12).
pub fn router(state: AppState) -> Router {
    let config = state.config();
    let store = crate::web::session::SqliteSessionStore::new(state.db.pool().clone());
    let session_layer = SessionManagerLayer::new(store)
        .with_name("daily_session")
        .with_http_only(true)
        .with_same_site(axum_login::tower_sessions::cookie::SameSite::Lax)
        .with_secure(config.server.public_url.starts_with("https://"))
        .with_expiry(Expiry::OnInactivity(time::Duration::days(i64::from(
            config.server.session_days,
        ))));
    let auth_layer = AuthManagerLayerBuilder::new(
        crate::web::session::Backend::new(state.db.clone()),
        session_layer,
    )
    .build();

    Router::new()
        .route("/r/{date}/{article_id}/{vote}", get(handle_rating))
        .route(crate::publish::OPDS_PATH, get(handle_opds))
        // OPDS browsers are typed into by hand on a 6" e-ink keyboard: serve the
        // same feed from the catalog root so a URL without the filename works.
        .route("/opds", get(handle_opds))
        .route("/opds/", get(handle_opds))
        .route("/files/epub/{name}", get(handle_epub_file))
        .route("/files/xtc/{name}", get(handle_xtc_file))
        .route("/healthz", get(handle_healthz))
        .route("/issues.json", get(handle_issues_json))
        .merge(crate::web::router(&config))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        // Immediately inside the auth layer, so what is left outside it is the
        // session load and save that `server_timing` reports as `sess`.
        .layer(from_fn(crate::web::route_boundary))
        .layer(auth_layer)
        .layer(from_fn_with_state(
            state.clone(),
            crate::web::session::require_same_origin,
        ))
        .layer(from_fn(crate::web::reject_early_data))
        .layer(from_fn(crate::web::security_headers))
        .layer(from_fn(crate::web::server_timing))
        .with_state(state)
}

/// `daily-epub serve` — bind, serve, graceful shutdown on SIGTERM (§3.12).
pub async fn serve(
    config: Config,
    config_path: Option<PathBuf>,
    db: Db,
) -> Result<(), ServerError> {
    if config.server.hmac_secret.is_none() {
        // Not fatal for the OPDS routes, but every rating link would 500.
        tracing::warn!("server.hmac_secret is unset — rating links will be rejected");
    }
    let mailer = crate::mail::Mailer::from_config(&config.mail).map_err(ServerError::Mail)?;
    let addr = config.server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| ServerError::Bind {
            addr: addr.clone(),
            source,
        })?;
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    tracing::info!(bind = %local, public_url = %config.server.public_url, "serving");

    let store = crate::web::session::SqliteSessionStore::new(db.pool().clone());
    if let Err(error) = store.delete_expired().await {
        tracing::warn!(%error, "could not delete expired sessions at startup");
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(error) = store.delete_expired().await {
                tracing::warn!(%error, "could not delete expired sessions");
            }
        }
    });

    // The Jobs page starts `daily-epub-job@<name>.service` through systemd +
    // polkit (§14); `server.jobs_enabled = false` swaps in the runner whose
    // page says so.
    let jobs: Arc<dyn crate::web::JobRunner> = if config.server.jobs_enabled {
        Arc::new(crate::jobs::SystemdRunner::default())
    } else {
        tracing::info!("server.jobs_enabled is false; the Jobs page cannot start units");
        Arc::new(crate::web::DisabledRunner)
    };
    let mut state = AppState::with_jobs(db, config, config_path, jobs);
    state.mailer = mailer;
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    tracing::info!("server stopped");
    Ok(())
}

/// Resolve on SIGTERM (systemd stop) or ctrl-c (§3.12, §3.15).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install the ctrl-c handler");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => tracing::info!("ctrl-c received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    t: String,
}

async fn handle_healthz() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ok",
    )
        .into_response()
}

/// `GET /issues.json` — the last [`ISSUES_JSON_LIMIT`] run reports, newest first (§3.12).
async fn handle_issues_json(State(state): State<AppState>) -> Response {
    let rows = match state.db.recent_reports(ISSUES_JSON_LIMIT).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "issues.json query failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }
    };
    let issues: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(date, report)| {
            let report = report
                .as_deref()
                .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "date": date.to_string(), "report": report })
        })
        .collect();
    match serde_json::to_string_pretty(&issues) {
        // Only publishing changes this; five minutes of staleness is fine (§3.12).
        Ok(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, PUBLIC_CACHE),
            ],
            body,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "serializing issues.json failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "serialization error").into_response()
        }
    }
}

/// `GET /r/{date}/{article_id}/{vote}?t=TOKEN` (§3.9).
async fn handle_rating(
    State(state): State<AppState>,
    Path((date, article_id, vote)): Path<(String, String, String)>,
    Query(query): Query<TokenQuery>,
) -> Response {
    let Ok(date) = date.parse::<Date>() else {
        tracing::warn!(%date, "rating link with a malformed date");
        return page(StatusCode::BAD_REQUEST, "Bad link — invalid date.", None);
    };
    let Ok(article_id) = article_id.parse::<ArticleId>() else {
        return page(StatusCode::BAD_REQUEST, "Bad link — invalid article.", None);
    };
    let Some(vote) = Vote::parse(&vote) else {
        return page(StatusCode::BAD_REQUEST, "Bad link — invalid vote.", None);
    };

    let config = state.config();
    let Some(secret) = config.server.hmac_secret.as_deref() else {
        tracing::error!("rating request but server.hmac_secret is unset");
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Server misconfigured.",
            None,
        );
    };
    if !verify_token(secret, date, article_id, vote, &query.t) {
        tracing::warn!(%date, article_id, vote = vote.as_str(), "rejected rating token");
        return page(StatusCode::FORBIDDEN, "Invalid link.", None);
    }

    let article = match state.db.get_article(article_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            tracing::warn!(article_id, "rating for an unknown article");
            return page(StatusCode::NOT_FOUND, "Unknown article.", None);
        }
        Err(e) => {
            tracing::error!(error = %e, article_id, "loading the rated article failed");
            return page(StatusCode::INTERNAL_SERVER_ERROR, "Database error.", None);
        }
    };

    let label = vote.event_label();
    let event = RatingEvent {
        id: 0,
        user_id: None,
        issue_date: Some(date),
        article_id,
        kind: "explicit".into(),
        source: "epub".into(),
        label: label.into(),
        value: vote.value(&config.curation.feedback),
        note: None,
        event_at: Timestamp::now(),
    };
    if let Err(error) = state.db.append_rating_event(&event).await {
        tracing::error!(%error, article_id, "recording the rating failed");
        return page(StatusCode::INTERNAL_SERVER_ERROR, "Database error.", None);
    }
    tracing::info!(
        %date,
        article_id,
        feed_id = article.feed_id,
        vote = vote.as_str(),
        title = %article.title,
        "recorded rating event"
    );

    let message = match vote {
        Vote::Loved => "Recorded: Loved it — thanks.".to_string(),
        Vote::Good => "Recorded: Good — thanks.".to_string(),
        Vote::NotForMe => "Recorded: Not for me — thanks.".to_string(),
        Vote::Slop => crate::rate::slop_message(article.author.as_deref()),
    };
    confirmation_page(StatusCode::OK, &message, &config, date, article_id, vote)
}

/// `GET /opds/daily.xml` — both EPUB editions of the last issues, newest first,
/// rendered from the publish directory on each request (§3.11).
async fn handle_opds(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = state.config();
    if let Some(challenge) = check_basic_auth(&config, &headers) {
        return challenge;
    }
    match crate::publish::build_opds(&state.db, &config).await {
        Ok(feed) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, OPDS_CONTENT_TYPE),
                (header::CACHE_CONTROL, PRIVATE_CACHE),
            ],
            feed,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "could not build the OPDS feed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CACHE_CONTROL, PRIVATE_CACHE)],
                "could not build the feed",
            )
                .into_response()
        }
    }
}

/// `GET /files/epub/{name}` — download one published EPUB; this is what the
/// OPDS acquisition links point at (§3.11).
async fn handle_epub_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    auth: crate::web::session::AuthSession,
) -> Response {
    let dir = state.config().publish.epub_dir.clone();
    serve_file(&state, &dir, &name, &headers, auth.user().await.is_some()).await
}

/// `GET /files/xtc/{name}` — download one XTC artifact (§3.11).
///
/// Not listed in the OPDS feed — CrossPoint's browser cannot acquire XTC — but
/// kept so the artifacts can still be fetched by URL for sideloading.
async fn handle_xtc_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    auth: crate::web::session::AuthSession,
) -> Response {
    let dir = state.config().publish.xtc_dir.clone();
    serve_file(&state, &dir, &name, &headers, auth.user().await.is_some()).await
}

/// Stream one file out of `dir`. A signed-in web session (any role) bypasses
/// the OPDS Basic auth; without one the existing Basic auth policy applies
/// unchanged, including "no credentials configured ⇒ public", so e-readers
/// following the OPDS feed keep working (§3.11, dashboard plan §8).
async fn serve_file(
    state: &AppState,
    dir: &FsPath,
    name: &str,
    headers: &HeaderMap,
    signed_in: bool,
) -> Response {
    let config = state.config();
    if !signed_in && let Some(challenge) = check_basic_auth(&config, headers) {
        return challenge;
    }
    let Some(path) = safe_join(dir, name) else {
        tracing::warn!(name, "rejected an unsafe file name");
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, PRIVATE_CACHE)],
            "bad file name",
        )
            .into_response();
    };
    // An XTCH issue is a pre-rendered page bitmap per page — ~100 MB for a full
    // day. Stream it rather than buffering the whole file per request (§3.11).
    let (file, len) = match tokio::fs::File::open(&path).await {
        Ok(file) => {
            let len = file.metadata().await.map(|m| m.len()).ok();
            (file, len)
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "file not found");
            return (
                StatusCode::NOT_FOUND,
                [(header::CACHE_CONTROL, PRIVATE_CACHE)],
                "not found",
            )
                .into_response();
        }
    };
    // CrossPoint dispatches on the saved file's extension, not on this header,
    // but Calibre and KOReader both use it.
    let content_type = if name.ends_with(".epub") {
        crate::publish::EPUB_CONTENT_TYPE
    } else {
        "application/octet-stream"
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // Downloads are cookie- or Basic-auth gated: a shared cache must never hold
    // one, whatever cache rule the CDN in front of us is configured with (§3.12).
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PRIVATE_CACHE),
    );
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        name.replace('"', "")
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // CrossPoint shows a progress bar only when it knows the size up front.
    if let Some(len) = len
        && let Ok(value) = HeaderValue::from_str(&len.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    (StatusCode::OK, headers, body).into_response()
}

/// Resolve `name` inside `dir`, rejecting anything that could escape it (§3.12).
///
/// The name must be a single, plain file name: no separators, no `..`, no
/// absolute paths, no hidden files, and — belt and braces — the joined path must
/// still live inside `dir` once resolved.
pub fn safe_join(dir: &FsPath, name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
    {
        return None;
    }
    let mut components = FsPath::new(name).components();
    let only = match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(c)), None) => c.to_owned(),
        _ => return None,
    };
    let candidate = dir.join(only);
    // When both sides resolve, require containment (defends against symlinked names).
    match (candidate.canonicalize(), dir.canonicalize()) {
        (Ok(resolved), Ok(root)) if !resolved.starts_with(&root) => None,
        _ => Some(candidate),
    }
}

// ---------------------------------------------------------------------------
// Basic auth (§3.11 optional OPDS credentials)
// ---------------------------------------------------------------------------

/// `Some(challenge_response)` when the request must be rejected, `None` when it
/// may proceed (including when no credentials are configured).
fn check_basic_auth(config: &Config, headers: &HeaderMap) -> Option<Response> {
    let (Some(user), Some(pass)) = (
        config.server.basic_auth_user.as_deref(),
        config.server.basic_auth_pass.as_deref(),
    ) else {
        return None;
    };
    let expected = BASE64.encode(format!("{user}:{pass}"));
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .map(str::trim)
        .unwrap_or_default();
    if constant_time_eq(expected.as_bytes(), supplied.as_bytes()) {
        return None;
    }
    tracing::warn!("rejected an unauthenticated OPDS request");
    let challenge =
        HeaderValue::from_str(&format!("Basic realm=\"{AUTH_REALM}\", charset=\"UTF-8\""))
            .unwrap_or(HeaderValue::from_static("Basic"));
    Some(
        (
            StatusCode::UNAUTHORIZED,
            [
                (header::WWW_AUTHENTICATE, challenge),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(PRIVATE_CACHE),
                ),
            ],
            "authentication required",
        )
            .into_response(),
    )
}

// ---------------------------------------------------------------------------
// Tiny e-ink pages (§3.9)
// ---------------------------------------------------------------------------

/// A self-contained response page — no external CSS, well under 1 KB, legible on
/// a 6" e-ink browser (§3.9).
fn page(status: StatusCode, message: &str, note: Option<&str>) -> Response {
    let body = page_html(message, note, None);
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn confirmation_page(
    status: StatusCode,
    message: &str,
    config: &Config,
    date: Date,
    article_id: ArticleId,
    selected: Vote,
) -> Response {
    let Some(secret) = config.server.hmac_secret.as_deref() else {
        return page(status, message, None);
    };
    let choices = [Vote::Loved, Vote::Good, Vote::NotForMe, Vote::Slop]
        .into_iter()
        .filter(|vote| *vote != selected)
        .map(|vote| (vote, vote.display()))
        .map(|(vote, label)| {
            let url = rating_url(&config.server.public_url, secret, date, article_id, vote);
            format!(
                "<a href=\"{}\">[ {} ]</a>",
                escape_attr(&url),
                escape(label)
            )
        })
        .collect::<Vec<_>>()
        .join(" &nbsp; ");
    let issue_url = format!(
        "{}/issues/{date}",
        config.server.public_url.trim_end_matches('/')
    );
    let body = page_html(
        message,
        None,
        Some(&format!(
            "<p><small>Change it: {choices}</small></p><p><small><a href=\"{}\">Open this issue on the site</a></small></p>",
            escape_attr(&issue_url)
        )),
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// The page markup itself: no external stylesheet, script, or images (§6.1).
fn page_html(message: &str, note: Option<&str>, extra_html: Option<&str>) -> String {
    let note = note
        .map(|n| format!("<p><small>{}</small></p>", escape(n)))
        .unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>The Daily EPUB</title>\
<style>body{{margin:3em auto;max-width:18em;padding:0 1em;text-align:center;\
font:1.3em/1.5 Georgia,serif}}small{{font-size:.65em}}</style>\
<p>{}</p>{}{}",
        escape(message),
        note,
        extra_html.unwrap_or_default()
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape(s).replace('\"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Article, ExtractMethod, SourceKind, SourceRef};

    fn date() -> Date {
        "2026-08-15".parse().unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// The shared fixture vector: the EPUB footer builder must produce the same
    /// token for these inputs (§3.9).
    const VECTOR_SECRET: &str = "test-secret";
    const VECTOR_TOKEN_UP: &str = "3b314cf7e6d8f50f";

    #[test]
    fn token_matches_the_shared_test_vector() {
        let loved = rating_token(VECTOR_SECRET, date(), 42, Vote::Loved);
        assert_eq!(loved, "cece96767d6c5f8a");
        assert_eq!(loved.len(), 16);
        // Down differs from up, and both verify.
        let down = rating_token(VECTOR_SECRET, date(), 42, Vote::NotForMe);
        assert_ne!(down, VECTOR_TOKEN_UP);
        assert!(verify_token(
            VECTOR_SECRET,
            date(),
            42,
            Vote::Loved,
            VECTOR_TOKEN_UP
        ));
        assert!(verify_token(
            VECTOR_SECRET,
            date(),
            42,
            Vote::NotForMe,
            &down
        ));
    }

    /// The links the EPUB footer embeds must verify here — this is the whole
    /// feedback loop in one assertion (§3.9).
    #[test]
    fn epub_footer_links_verify_against_this_server() {
        for (id, vote) in [(42, Vote::Loved), (1234, Vote::NotForMe)] {
            let from_epub = crate::epub::build::rating_url(
                "https://daily.hallada.net",
                VECTOR_SECRET,
                date(),
                id,
                vote,
            );
            assert_eq!(
                from_epub,
                rating_url("https://daily.hallada.net", VECTOR_SECRET, date(), id, vote)
            );
            let token = from_epub.rsplit("?t=").next().unwrap_or_default();
            assert!(
                verify_token(VECTOR_SECRET, date(), id, vote, token),
                "{from_epub}"
            );
        }
    }

    #[test]
    fn token_verification_rejects_tampering() {
        let t = rating_token(VECTOR_SECRET, date(), 42, Vote::Loved);
        assert!(!verify_token(VECTOR_SECRET, date(), 42, Vote::NotForMe, &t));
        assert!(!verify_token(VECTOR_SECRET, date(), 43, Vote::Loved, &t));
        assert!(!verify_token("other-secret", date(), 42, Vote::Loved, &t));
        assert!(!verify_token(
            VECTOR_SECRET,
            "2026-08-16".parse().unwrap(),
            42,
            Vote::Loved,
            &t
        ));
        assert!(!verify_token(VECTOR_SECRET, date(), 42, Vote::Loved, ""));
        assert!(!verify_token(
            VECTOR_SECRET,
            date(),
            42,
            Vote::Loved,
            &format!("{t}00")
        ));
    }

    #[test]
    fn rating_url_is_the_link_the_epub_embeds() {
        assert_eq!(
            rating_url(
                "https://daily.hallada.net/",
                VECTOR_SECRET,
                date(),
                42,
                Vote::Loved
            ),
            format!(
                "https://daily.hallada.net/r/2026-08-15/42/loved?t={}",
                rating_token(VECTOR_SECRET, date(), 42, Vote::Loved)
            )
        );
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let dir = FsPath::new("/var/lib/daily-epub/xtc");
        assert_eq!(
            safe_join(dir, "The Daily EPUB - 2026-08-15 (X4).xtch"),
            Some(dir.join("The Daily EPUB - 2026-08-15 (X4).xtch"))
        );
        for bad in [
            "",
            "..",
            "../secret",
            "..%2Fsecret",
            "a/../../secret",
            "sub/dir.xtch",
            "/etc/passwd",
            ".hidden",
            "back\\slash",
        ] {
            assert!(safe_join(dir, bad).is_none(), "should reject {bad:?}");
        }
        // Percent-decoding happens before us: a decoded traversal is rejected too.
        assert!(safe_join(dir, "../../etc/passwd").is_none());
    }

    #[tokio::test]
    async fn the_confirmation_page_is_tiny_and_self_contained() {
        let html = page_html(
            "Recorded: Loved it — thanks.",
            Some("2026-08-15 · article 42"),
            None,
        );
        assert!(html.len() < 1024, "page is {} bytes", html.len());
        assert!(!html.contains("<link"), "no external stylesheet");
        assert!(!html.contains("<script"), "no script");
        assert!(html.contains("Recorded: Loved it"));
        assert_eq!(
            page(StatusCode::FORBIDDEN, "Invalid link.", None).status(),
            StatusCode::FORBIDDEN
        );
        assert!(page_html("<b>x</b>", None, None).contains("&lt;b&gt;"));

        let mut config = Config::default();
        config.server.hmac_secret = Some("test-secret".into());
        config.server.public_url = "https://daily.example".into();
        let response =
            confirmation_page(StatusCode::OK, "Recorded", &config, date(), 42, Vote::Loved);
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            body.contains(
                "href=\"https://daily.example/issues/2026-08-15\">Open this issue on the site"
            ),
            "{body}"
        );
    }

    // -----------------------------------------------------------------
    // End-to-end over a real listener (the crate has no lib target, so the
    // HTTP-level tests live here rather than in `tests/`).
    // -----------------------------------------------------------------

    struct TestServer {
        base: String,
        db: Db,
        _dir: tempfile::TempDir,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start(with_auth: bool) -> TestServer {
            let dir = tempfile::tempdir().unwrap();
            let xtc_dir = dir.path().join("xtc");
            let epub_dir = dir.path().join("epub");
            std::fs::create_dir_all(&xtc_dir).unwrap();
            std::fs::create_dir_all(&epub_dir).unwrap();
            let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
                .await
                .unwrap();

            let mut config = Config::default();
            config.server.hmac_secret = Some(VECTOR_SECRET.into());
            config.publish.xtc_dir = xtc_dir;
            config.publish.epub_dir = epub_dir;
            config.server.public_url = "https://daily.hallada.net".into();
            if with_auth {
                config.server.basic_auth_user = Some("opds".into());
                config.server.basic_auth_pass = Some("hunter2".into());
            }

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(AppState::new(db.clone(), config, None));
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            TestServer {
                base: format!("http://{addr}"),
                db,
                _dir: dir,
                handle,
            }
        }

        fn xtc_dir(&self) -> PathBuf {
            self._dir.path().join("xtc")
        }

        fn epub_dir(&self) -> PathBuf {
            self._dir.path().join("epub")
        }

        async fn seed_article(&self) -> ArticleId {
            let entry = crate::types::Entry {
                id: 1,
                feed_id: 7,
                feed_title: Some("Hacker News".into()),
                category: None,
                title: "Story".into(),
                url: "https://example.com/1".into(),
                canonical_url: Some("https://example.com/1".into()),
                author: None,
                published_at: Some(ts("2026-08-15T04:00:00Z")),
                comments_url: None,
                raw_content: "<p>hi</p>".into(),
                fetched_at: ts("2026-08-15T05:30:00Z"),
            };
            self.db.upsert_entry(&entry).await.unwrap();
            let article = Article {
                id: 0,
                canonical_url: "https://example.com/1".into(),
                title: "Story".into(),
                best_entry_id: 1,
                content_html: "<p>hi</p>".into(),
                word_count: 500,
                excerpt_only: false,
                image_count: 0,
                sources: vec![SourceRef {
                    entry_id: 1,
                    feed_id: 7,
                    feed_title: "Hacker News".into(),
                    category: None,
                    kind: SourceKind::HnFrontpage,
                }],
                first_seen: ts("2026-08-15T05:30:00Z"),
                url: "https://example.com/1".into(),
                author: None,
                feed_id: 7,
                feed_title: "Hacker News".into(),
                category: None,
                published_at: None,
                comments_url: None,
                image_urls: vec![],
                social: vec![],
                extract_method: ExtractMethod::Miniflux,
            };
            self.db.upsert_article(&article).await.unwrap()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn healthz_and_issues_json() {
        let server = TestServer::start(false).await;
        let res = client()
            .get(format!("{}/healthz", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), "ok");

        let res = client()
            .get(format!("{}/issues.json", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body.as_array().map(Vec::len), Some(0));

        // Newest first, report JSON inlined.
        for (day, n) in [("2026-08-13", 1), ("2026-08-15", 3), ("2026-08-14", 2)] {
            server
                .db
                .upsert_issue(
                    day.parse().unwrap(),
                    n,
                    ts("2026-08-15T05:30:00Z"),
                    None,
                    None,
                    None,
                    None,
                    Some(&format!("{{\"selected\":{n}}}")),
                    None,
                )
                .await
                .unwrap();
        }
        let body: serde_json::Value = client()
            .get(format!("{}/issues.json", server.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let dates: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["date"].as_str().unwrap())
            .collect();
        assert_eq!(dates, ["2026-08-15", "2026-08-14", "2026-08-13"]);
        assert_eq!(body[0]["report"]["selected"], 3);
    }

    #[tokio::test]
    async fn rating_taps_append_events_and_latest_correction_wins() {
        let server = TestServer::start(false).await;
        let id = server.seed_article().await;
        let loved = rating_url(&server.base, VECTOR_SECRET, date(), id, Vote::Loved);

        let response = client().get(&loved).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert!(body.contains("Recorded: Loved it — thanks."), "{body}");
        assert!(body.contains("[ Good ]"), "{body}");
        assert!(body.contains("[ Not for me ]"), "{body}");
        assert!(
            body.len() < 2048,
            "confirmation page is {} bytes",
            body.len()
        );

        // Every tap is history, even a repeated one.
        assert_eq!(client().get(&loved).send().await.unwrap().status(), 200);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rating_events")
            .fetch_one(server.db.pool())
            .await
            .unwrap();
        assert_eq!(count, 2);

        // A correction appends and becomes the current verdict.
        let down = rating_url(&server.base, VECTOR_SECRET, date(), id, Vote::NotForMe);
        assert_eq!(client().get(&down).send().await.unwrap().status(), 200);
        let current = server.db.current_ratings(36500).await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].label, "not_for_me");
        let sources: Vec<String> =
            sqlx::query_scalar("SELECT source FROM rating_events ORDER BY id")
                .fetch_all(server.db.pool())
                .await
                .unwrap();
        assert_eq!(sources, ["epub", "epub", "epub"]);
    }

    #[tokio::test]
    async fn rating_rejects_bad_tokens_dates_and_unknown_articles() {
        let server = TestServer::start(false).await;
        let id = server.seed_article().await;

        let bad = format!("{}/r/2026-08-15/{id}/up?t=deadbeefdeadbeef", server.base);
        assert_eq!(client().get(&bad).send().await.unwrap().status(), 403);
        let missing = format!("{}/r/2026-08-15/{id}/up", server.base);
        assert_eq!(client().get(&missing).send().await.unwrap().status(), 403);

        // A valid token for an article that does not exist.
        let unknown = rating_url(&server.base, VECTOR_SECRET, date(), 9999, Vote::Loved);
        assert_eq!(client().get(&unknown).send().await.unwrap().status(), 404);

        // Malformed date / vote.
        let token = rating_token(VECTOR_SECRET, date(), id, Vote::Loved);
        let bad_date = format!("{}/r/not-a-date/{id}/up?t={token}", server.base);
        assert_eq!(client().get(&bad_date).send().await.unwrap().status(), 400);
        let bad_vote = format!("{}/r/2026-08-15/{id}/sideways?t={token}", server.base);
        assert_eq!(client().get(&bad_vote).send().await.unwrap().status(), 400);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rating_events")
            .fetch_one(server.db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn opds_and_files_are_served_behind_basic_auth() {
        let server = TestServer::start(true).await;
        for edition in crate::types::Edition::ALL {
            let name = crate::publish::issue_filename(date(), edition, "epub");
            std::fs::write(server.epub_dir().join(name), b"EPUB").unwrap();
        }
        std::fs::write(
            server
                .xtc_dir()
                .join("The Daily EPUB - 2026-08-15 (X4).xtch"),
            b"XTCH",
        )
        .unwrap();

        let res = client()
            .get(format!("{}/opds/daily.xml", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
        assert!(
            res.headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .starts_with("Basic realm=")
        );

        let res = client()
            .get(format!("{}/opds/daily.xml", server.base))
            .basic_auth("opds", Some("wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        let res = client()
            .get(format!("{}/opds/daily.xml", server.base))
            .basic_auth("opds", Some("hunter2"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(
            res.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/atom+xml")
        );
        let feed = res.text().await.unwrap();
        assert_eq!(feed.matches("<entry>").count(), 2, "{feed}");
        assert_eq!(feed.matches("application/epub+zip").count(), 2, "{feed}");
        // XTC exists on disk but is never advertised (§3.11).
        assert!(!feed.contains(".xtch"), "{feed}");

        // The catalog root serves the same feed, behind the same auth.
        for alias in ["/opds", "/opds/"] {
            let res = client()
                .get(format!("{}{alias}", server.base))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 401, "{alias}");
            let res = client()
                .get(format!("{}{alias}", server.base))
                .basic_auth("opds", Some("hunter2"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200, "{alias}");
            assert_eq!(res.text().await.unwrap(), feed, "{alias}");
        }

        // The acquisition link resolves, typed as an EPUB.
        let res = client()
            .get(format!(
                "{}/files/epub/The%20Daily%20EPUB%20-%202026-08-15%20(X4).epub",
                server.base
            ))
            .basic_auth("opds", Some("hunter2"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        // CrossPoint needs the size up front to show download progress.
        assert_eq!(res.content_length(), Some(4));
        assert_eq!(
            res.headers()[header::CONTENT_TYPE].to_str().unwrap(),
            "application/epub+zip"
        );
        assert_eq!(res.bytes().await.unwrap().as_ref(), b"EPUB");

        // XTC is still fetchable by URL for sideloading, just not listed.
        let res = client()
            .get(format!(
                "{}/files/xtc/The%20Daily%20EPUB%20-%202026-08-15%20(X4).xtch",
                server.base
            ))
            .basic_auth("opds", Some("hunter2"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.bytes().await.unwrap().as_ref(), b"XTCH");

        // Ratings are not behind auth (the token is the credential).
        assert_eq!(
            client()
                .get(format!("{}/healthz", server.base))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }

    #[tokio::test]
    async fn file_route_rejects_path_traversal() {
        let server = TestServer::start(false).await;
        std::fs::write(server._dir.path().join("secret"), b"top secret").unwrap();

        // Encoded traversal survives URL normalization and reaches the handler.
        let res = client()
            .get(format!("{}/files/xtc/..%2Fsecret", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        let res = client()
            .get(format!(
                "{}/files/xtc/%2e%2e%2f%2e%2e%2fsecret",
                server.base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        // A plain `..` segment is not even a match for the single-segment route.
        let res = client()
            .get(format!("{}/files/xtc/nope.xtch", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }
}
