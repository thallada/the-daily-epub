//! Dashboard: the profile page (`/dashboard/profile`, web plan §11).
//!
//! Edits `profile.md` with version history, shows what the loader parses out
//! of it, the standing OPML interests by theme, the stored system prompt and
//! the weekly learned adjustments, and offers the `profile-rebuild` job.

use std::path::Path;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Form, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use serde::Deserialize;
use sqlx::Row;

use crate::curate::profile::{self, KV_LEARNED_ADJUSTMENTS, ProfileFile, REBUILD_INTERVAL_DAYS};
use crate::db::{Db, DbError, KV_TASTE_PROFILE};
use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Flash, Html, Page, WebError, format_time, take_flash};

/// Largest `profile.md` the editor accepts (§11).
pub const MAX_PROFILE_BYTES: usize = 64 * 1024;
const PREVIEW_CHARS: usize = 200;
const VERSIONS_SHOWN: i64 = 50;

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/profile", get(show).post(save))
        .route("/dashboard/profile/restore", post(restore))
}

// ---------------------------------------------------------------------------
// File handling
// ---------------------------------------------------------------------------

/// Normalize a submitted profile: browser textareas send CRLF, the file is LF.
pub fn normalize(content: &str) -> String {
    let mut text = content.replace("\r\n", "\n").replace('\r', "\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Reject empty or oversized profiles (§11); everything else parses.
pub fn validate(content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("the profile cannot be empty".into());
    }
    if content.len() > MAX_PROFILE_BYTES {
        return Err(format!(
            "the profile is {} bytes; the limit is {} bytes",
            content.len(),
            MAX_PROFILE_BYTES
        ));
    }
    Ok(())
}

/// Write `<path>.tmp` and rename it over `path`, keeping the existing file's
/// permissions (§11).
pub fn write_atomically(path: &Path, content: &str) -> anyhow::Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "profile.md".into());
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    std::fs::write(&tmp, content)
        .map_err(|error| anyhow::anyhow!("writing {}: {error}", tmp.display()))?;
    if let Some(permissions) = permissions
        && let Err(error) = std::fs::set_permissions(&tmp, permissions)
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!(
            "preserving permissions on {}: {error}",
            tmp.display()
        ));
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!(
            "renaming {} over {}: {error}",
            tmp.display(),
            path.display()
        ));
    }
    Ok(())
}

fn read_profile(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!("reading {}: {error}", path.display())),
    }
}

/// The live preview of what the loader extracts (§11): the passthrough body
/// and the `## Interests` lines.
pub fn preview(content: &str) -> ProfileFile {
    profile::parse_profile_str(content)
}

fn short_preview(content: &str) -> String {
    let mut out: String = content.chars().take(PREVIEW_CHARS).collect();
    if content.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

async fn record_version(
    db: &Db,
    content: &str,
    saved_by: i64,
    now: Timestamp,
) -> Result<i64, WebError> {
    let row = sqlx::query(
        "INSERT INTO profile_versions (content, saved_by, saved_at) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(content)
    .bind(saved_by)
    .bind(crate::db::fmt_ts(now))
    .fetch_one(db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(row.get("id"))
}

async fn version_content(db: &Db, id: i64) -> Result<Option<String>, WebError> {
    let row = sqlx::query("SELECT content FROM profile_versions WHERE id = ?")
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .map_err(DbError::from)?;
    Ok(row.map(|row| row.get("content")))
}

struct VersionView {
    id: i64,
    saved_at: String,
    saved_by: String,
    bytes: usize,
    preview: String,
}

async fn versions(db: &Db, config: &crate::config::Config) -> Result<Vec<VersionView>, WebError> {
    let rows = sqlx::query(
        "SELECT pv.id, pv.content, pv.saved_at, u.username
         FROM profile_versions pv LEFT JOIN users u ON u.id = pv.saved_by
         ORDER BY pv.id DESC LIMIT ?",
    )
    .bind(VERSIONS_SHOWN)
    .fetch_all(db.pool())
    .await
    .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let content: String = row.get("content");
        let saved_at = crate::db::parse_ts(
            "profile_versions.saved_at",
            &row.get::<String, _>("saved_at"),
        )?;
        out.push(VersionView {
            id: row.get("id"),
            saved_at: format_time(saved_at, config),
            saved_by: row
                .get::<Option<String>, _>("username")
                .unwrap_or_else(|| "—".into()),
            bytes: content.len(),
            preview: short_preview(&content),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

struct ThemeView {
    name: String,
    members: String,
    count: usize,
}

#[derive(Template)]
#[template(path = "dashboard/profile.html")]
struct ProfileTemplate {
    page: Page,
    path: String,
    exists: bool,
    content: String,
    bytes: usize,
    max_bytes: usize,
    preview_body: String,
    preview_interests: Vec<String>,
    versions: Vec<VersionView>,
    opml_path: String,
    opml_count: usize,
    opml_error: String,
    themes: Vec<ThemeView>,
    prompt: String,
    prompt_chars: usize,
    prompt_version: String,
    prompt_built_at: String,
    prompt_verdicts: usize,
    learned: String,
    learned_age: String,
    rebuild_interval_days: i64,
    rebuild_due: bool,
    jobs_enabled: bool,
}

fn count_verdict_lines(prompt: &str) -> usize {
    prompt
        .split_once("## Recent verdicts")
        .map(|(_, rest)| rest.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

async fn show(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: "/dashboard/profile".into(),
        })?;
    let config = state.config();
    let db = &state.db;
    let now = Timestamp::now();

    let path = config.profile_path.clone();
    let stored = read_profile(&path).map_err(WebError::Internal)?;
    let exists = stored.is_some();
    let content = stored.unwrap_or_default();
    let parsed = preview(&content);

    let (opml_count, opml_error, themes) = match profile::parse_interests(&config.interests_opml) {
        Ok(interests) => {
            let themes = profile::group_into_themes(&interests)
                .into_iter()
                .map(|(name, members)| ThemeView {
                    name,
                    count: members.len(),
                    members: members.join(", "),
                })
                .collect();
            (interests.len(), String::new(), themes)
        }
        Err(error) => (0, format!("{error:#}"), Vec::new()),
    };

    let prompt = db.kv_get(KV_TASTE_PROFILE).await?.unwrap_or_default();
    let learned = db.kv_get(KV_LEARNED_ADJUSTMENTS).await?.unwrap_or_default();
    let version = profile::stored_version(db)
        .await
        .map_err(WebError::Internal)?;
    let (prompt_version, prompt_built_at, learned_age) = match version {
        Some((version, built_at)) => {
            let age_days = (now.as_second() - built_at.as_second()).max(0) / 86_400;
            (
                version.to_string(),
                format_time(built_at, &config),
                format!("{age_days} days old"),
            )
        }
        None => ("—".into(), "never".into(), "never built".into()),
    };
    let rebuild_due = profile::is_stale(db).await.map_err(WebError::Internal)?;

    let mut page = Page::new("Profile", Some(viewer), "profile");
    page.flash = take_flash(&session).await?;
    Ok(Html(ProfileTemplate {
        page,
        path: path.display().to_string(),
        exists,
        bytes: content.len(),
        max_bytes: MAX_PROFILE_BYTES,
        content,
        preview_body: parsed.body,
        preview_interests: parsed.interests,
        versions: versions(db, &config).await?,
        opml_path: config.interests_opml.display().to_string(),
        opml_count,
        opml_error,
        themes,
        prompt_chars: prompt.len(),
        prompt_verdicts: count_verdict_lines(&prompt),
        prompt,
        prompt_version,
        prompt_built_at,
        learned,
        learned_age,
        rebuild_interval_days: REBUILD_INTERVAL_DAYS,
        rebuild_due,
        jobs_enabled: config.server.jobs_enabled,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Save and restore
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SaveForm {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
pub struct RestoreForm {
    version_id: i64,
}

async fn flash_and_redirect(
    session: &Session,
    kind: &str,
    text: String,
) -> Result<Response, WebError> {
    session
        .insert(
            "flash",
            Flash {
                kind: kind.into(),
                text,
            },
        )
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    Ok(Redirect::to("/dashboard/profile").into_response())
}

/// Replace `profile.md` with `content`, recording the previous text as a
/// version. Returns the new version row's id when one was written.
async fn replace_profile(
    state: &AppState,
    viewer: &Viewer,
    content: &str,
) -> Result<Option<i64>, WebError> {
    let config = state.config();
    let path = config.profile_path.clone();
    let previous = read_profile(&path).map_err(WebError::Internal)?;
    let version = match previous {
        Some(previous) if previous != content => {
            Some(record_version(&state.db, &previous, viewer.id, Timestamp::now()).await?)
        }
        _ => None,
    };
    write_atomically(&path, content).map_err(WebError::Internal)?;
    Ok(version)
}

async fn save(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Form(form): Form<SaveForm>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: "/dashboard/profile".into(),
        })?;
    let content = normalize(&form.content);
    validate(&content).map_err(WebError::BadRequest)?;
    let config = state.config();
    let current = read_profile(&config.profile_path).map_err(WebError::Internal)?;
    if current.as_deref() == Some(content.as_str()) {
        return flash_and_redirect(&session, "info", "No changes to save.".into()).await;
    }
    replace_profile(&state, &viewer, &content).await?;
    tracing::info!(
        user = %viewer.username,
        bytes = content.len(),
        path = %config.profile_path.display(),
        "profile.md saved from the dashboard"
    );
    flash_and_redirect(
        &session,
        "success",
        "Saved; the next run rebuilds the system prompt.".into(),
    )
    .await
}

async fn restore(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Form(form): Form<RestoreForm>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: "/dashboard/profile".into(),
        })?;
    let content = version_content(&state.db, form.version_id)
        .await?
        .ok_or(WebError::NotFound)?;
    validate(&content).map_err(WebError::BadRequest)?;
    replace_profile(&state, &viewer, &content).await?;
    tracing::info!(
        user = %viewer.username,
        version = form.version_id,
        "profile.md restored from the dashboard"
    );
    flash_and_redirect(
        &session,
        "success",
        format!(
            "Restored version #{}; the next run rebuilds the system prompt.",
            form.version_id
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;
    use crate::web::users;

    #[test]
    fn preview_matches_the_loader() {
        let raw = "# Reader profile\n\n## Who\nProse.\n\n## Interests\n- Rust\nBoston\n\n## Notes\nKeep.\n";
        let parsed = preview(raw);
        assert_eq!(parsed, profile::parse_profile_str(raw));
        assert_eq!(
            parsed.body,
            "# Reader profile\n\n## Who\nProse.\n\n## Notes\nKeep.\n"
        );
        assert_eq!(parsed.interests, ["Rust", "Boston"]);
    }

    #[test]
    fn normalize_and_validate_bound_the_profile() {
        assert_eq!(normalize("a\r\nb"), "a\nb\n");
        assert_eq!(normalize("a\n"), "a\n");
        assert!(validate("   \n").is_err());
        assert!(validate("# ok\n").is_ok());
        let big = "x".repeat(MAX_PROFILE_BYTES + 1);
        assert!(validate(&big).is_err());
        assert!(validate(&"x".repeat(MAX_PROFILE_BYTES)).is_ok());
        assert_eq!(short_preview("short"), "short");
        let long = "y".repeat(PREVIEW_CHARS + 5);
        assert_eq!(short_preview(&long).chars().count(), PREVIEW_CHARS + 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_mode_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.md");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomically(&path, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.path().join("profile.md.tmp").exists());
        // A missing file is created rather than failing.
        let fresh = dir.path().join("fresh.md");
        write_atomically(&fresh, "created\n").unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "created\n");
    }

    #[test]
    fn verdict_lines_are_counted_from_the_prompt() {
        assert_eq!(count_verdict_lines("no block"), 0);
        assert_eq!(
            count_verdict_lines("intro\n\n## Recent verdicts\n\nLOVED | a\nGOOD | b\n"),
            2
        );
    }

    async fn setup() -> (tempfile::TempDir, AppState, axum::Router, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        users::add(&db, "tyler", "correct horse battery", true)
            .await
            .unwrap();
        users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let config = Config {
            profile_path: dir.path().join("profile.md"),
            interests_opml: dir.path().join("interests.opml"),
            ..Config::default()
        };
        std::fs::write(&config.profile_path, "# Original\n\nProse.\n").unwrap();
        std::fs::write(
            &config.interests_opml,
            r#"<outline text="Rust"/><outline text="Boston"/>"#,
        )
        .unwrap();
        db.kv_set(
            KV_TASTE_PROFILE,
            "system prompt text\n\n## Recent verdicts\n\nLOVED | x\n",
        )
        .await
        .unwrap();
        db.kv_set(KV_LEARNED_ADJUSTMENTS, "- Rank depth higher.")
            .await
            .unwrap();
        let state = AppState::new(db, config, None);
        let app = crate::server::router(state.clone());
        let cookie = login(&app, "tyler").await;
        (dir, state, app, cookie)
    }

    async fn login(app: &axum::Router, username: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.45")
                    .body(Body::from(format!(
                        "username={username}&password=correct+horse+battery&next=%2F"
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

    async fn get(app: &axum::Router, cookie: Option<&str>) -> Response {
        let mut request = Request::builder().uri("/dashboard/profile");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn post(app: &axum::Router, uri: &str, body: String, cookie: Option<&str>) -> Response {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(request.body(Body::from(body)).unwrap())
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

    async fn version_rows(db: &Db) -> Vec<(i64, String, Option<i64>)> {
        sqlx::query("SELECT id, content, saved_by FROM profile_versions ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get("id"), row.get("content"), row.get("saved_by")))
            .collect()
    }

    #[tokio::test]
    async fn profile_page_shows_editor_preview_interests_prompt_and_rebuild_form() {
        let (_dir, _state, app, cookie) = setup().await;
        let response = get(&app, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = text(response).await;
        assert!(body.contains("# Original"));
        assert!(body.contains("Prose."));
        assert!(body.contains("Rust, Boston") || body.contains("Rust") && body.contains("Boston"));
        assert!(body.contains("2 interests"));
        assert!(body.contains("system prompt text"));
        assert!(body.contains("Rank depth higher."));
        assert!(body.contains("never built"));
        assert!(body.contains("rebuild is due"));
        assert!(body.contains(r#"action="/dashboard/jobs/profile-rebuild""#));
        assert!(body.contains("No saved versions yet"));
    }

    #[tokio::test]
    async fn save_writes_the_file_and_records_the_previous_version() {
        let (_dir, state, app, cookie) = setup().await;
        let admin = users::find_by_username(&state.db, "tyler")
            .await
            .unwrap()
            .unwrap();
        let response = post(
            &app,
            "/dashboard/profile",
            "content=%23+New%0D%0A%0D%0A%23%23+Interests%0D%0A-+Writerdeck%0D%0A".into(),
            Some(&cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/dashboard/profile"
        );
        let path = state.config().profile_path.clone();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# New\n\n## Interests\n- Writerdeck\n"
        );
        assert!(!path.with_file_name("profile.md.tmp").exists());
        let rows = version_rows(&state.db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "# Original\n\nProse.\n");
        assert_eq!(rows[0].2, Some(admin.id));

        let page = text(get(&app, Some(&cookie)).await).await;
        assert!(page.contains("Saved; the next run rebuilds the system prompt."));
        assert!(page.contains("Writerdeck"));
        assert!(page.contains("# Original"));
        assert!(page.contains(">tyler<"));

        // Saving identical content records nothing.
        let same = post(
            &app,
            "/dashboard/profile",
            "content=%23+New%0A%0A%23%23+Interests%0A-+Writerdeck%0A".into(),
            Some(&cookie),
        )
        .await;
        assert_eq!(same.status(), StatusCode::SEE_OTHER);
        assert_eq!(version_rows(&state.db).await.len(), 1);
    }

    #[tokio::test]
    async fn restore_swaps_the_file_and_records_the_current_one() {
        let (_dir, state, app, cookie) = setup().await;
        post(
            &app,
            "/dashboard/profile",
            "content=%23+Second%0A".into(),
            Some(&cookie),
        )
        .await;
        let first = version_rows(&state.db).await[0].0;
        let response = post(
            &app,
            "/dashboard/profile/restore",
            format!("version_id={first}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let path = state.config().profile_path.clone();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Original\n\nProse.\n"
        );
        let rows = version_rows(&state.db).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].1, "# Second\n");
        let missing = post(
            &app,
            "/dashboard/profile/restore",
            "version_id=999".into(),
            Some(&cookie),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn empty_and_oversized_profiles_are_rejected_and_the_file_is_untouched() {
        let (_dir, state, app, cookie) = setup().await;
        let empty = post(
            &app,
            "/dashboard/profile",
            "content=+%0A".into(),
            Some(&cookie),
        )
        .await;
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        let big = format!("content={}", "x".repeat(MAX_PROFILE_BYTES + 10));
        let oversized = post(&app, "/dashboard/profile", big, Some(&cookie)).await;
        assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read_to_string(&state.config().profile_path).unwrap(),
            "# Original\n\nProse.\n"
        );
        assert!(version_rows(&state.db).await.is_empty());
    }

    #[tokio::test]
    async fn profile_routes_are_admin_only() {
        let (_dir, state, app, _admin) = setup().await;
        let anonymous = get(&app, None).await;
        assert_eq!(anonymous.status(), StatusCode::FOUND);
        assert_eq!(
            anonymous.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fdashboard%2Fprofile"
        );
        let reader = login(&app, "reader").await;
        assert_eq!(
            get(&app, Some(&reader)).await.status(),
            StatusCode::FORBIDDEN
        );
        let save = post(
            &app,
            "/dashboard/profile",
            "content=%23+Hacked%0A".into(),
            Some(&reader),
        )
        .await;
        assert_eq!(save.status(), StatusCode::FORBIDDEN);
        let restore = post(
            &app,
            "/dashboard/profile/restore",
            "version_id=1".into(),
            Some(&reader),
        )
        .await;
        assert_eq!(restore.status(), StatusCode::FORBIDDEN);
        let anonymous_save = post(
            &app,
            "/dashboard/profile",
            "content=%23+Hacked%0A".into(),
            None,
        )
        .await;
        assert_eq!(anonymous_save.status(), StatusCode::FOUND);
        assert_eq!(
            std::fs::read_to_string(&state.config().profile_path).unwrap(),
            "# Original\n\nProse.\n"
        );
    }
}
