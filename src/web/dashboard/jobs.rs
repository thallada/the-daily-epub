//! Dashboard: the Jobs pages (`/dashboard/jobs`, dashboard plan §14.4).
//!
//! The catalogue as cards, the `jobs` table, `POST /dashboard/jobs/{name}`
//! (insert `requested`, ask the runner to start the unit) and the job page
//! with the live unit status and the journal tail. The server never runs the
//! pipeline in-process: `job run` inside the unit does the work (§14.2).

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use jiff::Timestamp;

use crate::config::Config;
use crate::jobs::{self, EXITED_BEFORE_START, Job, JobRow};
use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Flash, Html, Page, UnitStatus, WebError, take_flash};

use super::{db_err, duration_between, fmt_duration, fmt_stored_time};

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/jobs", get(index))
        .route("/dashboard/jobs/{key}", get(show).post(start))
}

/// How many rows the jobs table shows.
const TABLE_ROWS: i64 = 200;

/// A `requested` row whose unit already stopped with a failure this long after
/// the request is marked failed (§14.4).
const EXIT_GRACE_SECS: i64 = 30;

#[derive(Debug, Clone)]
struct JobCard {
    name: String,
    description: &'static str,
    dangerous: bool,
    /// The `generate` card carries the date input for `generate-YYYY-MM-DD`.
    dated: bool,
    lock: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct JobLine {
    id: i64,
    name: String,
    requested_by: String,
    requested: String,
    started: String,
    finished: String,
    duration: String,
    status: String,
    message: Option<String>,
    run_id: Option<i64>,
}

#[derive(Template)]
#[template(path = "dashboard/jobs.html")]
struct JobsTemplate {
    page: Page,
    jobs_enabled: bool,
    cards: Vec<JobCard>,
    jobs: Vec<JobLine>,
    today: String,
}

#[derive(Template)]
#[template(path = "dashboard/job.html")]
struct JobTemplate {
    page: Page,
    job: JobLine,
    unit: String,
    description: &'static str,
    refresh: bool,
    status: Option<UnitStatus>,
    status_error: Option<String>,
    log: String,
    log_error: Option<String>,
    log_lines: u32,
}

fn cards() -> Vec<JobCard> {
    Job::CATALOGUE
        .iter()
        .map(|job| JobCard {
            name: job.name(),
            description: job.description(),
            dangerous: job.dangerous(),
            dated: matches!(job, Job::Generate { .. }),
            lock: job.takes_lock(),
        })
        .collect()
}

fn job_line(row: &JobRow, config: &Config) -> JobLine {
    JobLine {
        id: row.id,
        name: row.name.clone(),
        requested_by: match (&row.requested_by_name, row.requested_by) {
            (Some(name), _) => name.clone(),
            (None, Some(id)) => format!("user {id}"),
            (None, None) => "by hand".into(),
        },
        requested: fmt_stored_time(Some(&row.requested_at), config),
        started: fmt_stored_time(row.started_at.as_deref(), config),
        finished: fmt_stored_time(row.finished_at.as_deref(), config),
        duration: fmt_duration(
            row.started_at
                .as_deref()
                .and_then(|started| duration_between(started, row.finished_at.as_deref())),
        ),
        status: row.status.clone(),
        message: row.message.clone(),
        run_id: row.run_id,
    }
}

async fn jobs_template(
    state: &AppState,
    viewer: Option<Viewer>,
    flash: Option<Flash>,
) -> Result<JobsTemplate, WebError> {
    let config = state.config();
    let rows = jobs::list(&state.db, TABLE_ROWS).await.map_err(db_err)?;
    let mut page = Page::new("Jobs", viewer, "dashboard");
    page.flash = flash;
    let today = config
        .tz()
        .map(|tz| Timestamp::now().to_zoned(tz).date().to_string())
        .unwrap_or_else(|_| {
            Timestamp::now()
                .to_zoned(jiff::tz::TimeZone::UTC)
                .date()
                .to_string()
        });
    Ok(JobsTemplate {
        page,
        jobs_enabled: config.server.jobs_enabled,
        cards: cards(),
        jobs: rows.iter().map(|row| job_line(row, &config)).collect(),
        today,
    })
}

/// `GET /dashboard/jobs`: the catalogue cards and the jobs table.
async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let flash = take_flash(&session).await?;
    Ok(Html(jobs_template(&state, viewer, flash).await?).into_response())
}

/// The `date` field of the start form, when present and non-empty.
fn form_date(body: &[u8]) -> Option<String> {
    url::form_urlencoded::parse(body)
        .find(|(key, _)| key == "date")
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

/// `POST /dashboard/jobs/{name}` (admin, origin-checked): parse the name,
/// refuse a duplicate `requested`/`running` unit (409), insert `requested`,
/// start the unit; a failed start marks the row `failed`.
async fn start(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: "/dashboard/jobs".into(),
        })?;
    let mut job = Job::parse(&name).ok_or(WebError::NotFound)?;
    if let (Job::Generate { date: None }, Some(date)) = (job, form_date(&body)) {
        let parsed = date
            .parse::<jiff::civil::Date>()
            .map_err(|_| WebError::BadRequest(format!("invalid date {date:?}")))?;
        job = Job::Generate { date: Some(parsed) };
    }
    let config = state.config();
    if !config.server.jobs_enabled {
        set_flash(
            &session,
            "error",
            "Jobs are disabled on this server (server.jobs_enabled = false).".into(),
        )
        .await?;
        return Ok(Redirect::to("/dashboard/jobs").into_response());
    }
    let unit = job.unit();
    if let Some(active) = jobs::active_for_unit(&state.db, &unit)
        .await
        .map_err(db_err)?
    {
        let flash = Flash {
            kind: "error".into(),
            text: format!(
                "{} is already requested or running (job {active}).",
                job.name()
            ),
        };
        let template = jobs_template(&state, Some(viewer), Some(flash)).await?;
        return Ok((StatusCode::CONFLICT, Html(template)).into_response());
    }

    // TODO(step 5 merge): reload_if_changed — pick up a hand-edited config.toml
    // before the unit starts (§4.2); step 5 adds the helper in web/mod.rs.
    let now = Timestamp::now();
    let id = jobs::insert_requested(&state.db, &job, Some(viewer.id), now)
        .await
        .map_err(db_err)?;
    match state.web.jobs.start(&unit).await {
        Ok(()) => {
            tracing::info!(user = %viewer.username, job = %job.name(), job_id = id, %unit, "job requested");
            set_flash(&session, "success", format!("Started {}.", job.name())).await?;
        }
        Err(error) => {
            tracing::warn!(user = %viewer.username, job = %job.name(), job_id = id, %unit, %error, "job start failed");
            jobs::finish(
                &state.db,
                id,
                jobs::Outcome::Failed,
                &format!("could not start {unit}: {error}"),
                None,
                Timestamp::now(),
            )
            .await
            .map_err(db_err)?;
            set_flash(
                &session,
                "error",
                format!("Could not start {}: {error}", job.name()),
            )
            .await?;
        }
    }
    Ok(Redirect::to(&format!("/dashboard/jobs/{id}")).into_response())
}

/// `GET /dashboard/jobs/{id}`: the row, the live unit status and the journal
/// tail; applies the 30-second "unit exited before the job started" rule.
async fn show(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(key): Path<String>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let id: i64 = key.parse().map_err(|_| WebError::NotFound)?;
    let config = state.config();
    let db = &state.db;
    let mut row = jobs::get(db, id)
        .await
        .map_err(db_err)?
        .ok_or(WebError::NotFound)?;
    let job = Job::parse(&row.name);
    let (status, status_error) = match state.web.jobs.status(&row.unit).await {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(error)),
    };
    if let Some(status) = &status
        && row.status == "requested"
        && status.exited_unsuccessfully()
        && requested_secs_ago(&row, Timestamp::now()) >= EXIT_GRACE_SECS
    {
        jobs::finish(
            db,
            id,
            jobs::Outcome::Failed,
            EXITED_BEFORE_START,
            None,
            Timestamp::now(),
        )
        .await
        .map_err(db_err)?;
        row = jobs::get(db, id)
            .await
            .map_err(db_err)?
            .ok_or(WebError::NotFound)?;
    }
    let lines = config.server.journal_lines;
    let (log, log_error) = match state.web.jobs.log(&row.unit, lines as usize).await {
        Ok(log) => (log, None),
        Err(error) => (String::new(), Some(error)),
    };
    let mut page = Page::new(format!("Job {id} · {}", row.name), viewer, "dashboard");
    page.flash = take_flash(&session).await?;
    Ok(Html(JobTemplate {
        page,
        refresh: row.is_active(),
        unit: row.unit.clone(),
        description: job.map(|job| job.description()).unwrap_or(""),
        job: job_line(&row, &config),
        status,
        status_error,
        log,
        log_error,
        log_lines: lines,
    })
    .into_response())
}

fn requested_secs_ago(row: &JobRow, now: Timestamp) -> i64 {
    row.requested_at
        .parse::<Timestamp>()
        .map(|requested| now.as_second() - requested.as_second())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    use super::*;
    use crate::db::Db;
    use crate::server::router;
    use crate::web::MockRunner;
    use crate::web::dashboard::tests::{assert_admin_only, get, login_cookie, response_text};

    async fn app_with_runner(
        config: Config,
        runner: Arc<dyn crate::web::JobRunner>,
    ) -> (tempfile::TempDir, Db, axum::Router) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        crate::web::users::add(&db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        let app = router(AppState::with_jobs(db.clone(), config, None, runner));
        (dir, db, app)
    }

    async fn post(app: &axum::Router, uri: &str, body: &str, cookie: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn location(response: &Response) -> String {
        response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn form_date_reads_the_optional_field() {
        assert_eq!(form_date(b"date=2026-09-03"), Some("2026-09-03".into()));
        assert_eq!(form_date(b"date=+2026-09-03+"), Some("2026-09-03".into()));
        assert_eq!(form_date(b"date="), None);
        assert_eq!(form_date(b""), None);
        assert_eq!(form_date(b"other=1"), None);
    }

    #[tokio::test]
    async fn jobs_page_lists_the_catalogue_and_the_table() {
        let runner = Arc::new(MockRunner::default());
        let (_dir, db, app) = app_with_runner(Config::default(), runner).await;
        let now: Timestamp = "2026-09-03T10:00:00Z".parse().unwrap();
        let id = jobs::insert_requested(&db, &Job::FeaturesPrune, None, now)
            .await
            .unwrap();
        jobs::finish(&db, id, jobs::Outcome::Ok, "pruned 0 embeddings", None, now)
            .await
            .unwrap();
        let body = assert_admin_only(&app, "/dashboard/jobs").await;
        for job in Job::CATALOGUE {
            assert!(
                body.contains(&format!("action=\"/dashboard/jobs/{}\"", job.name())),
                "{}: {body}",
                job.name()
            );
            let escaped_description = job.description().replace('\'', "&#39;");
            assert!(body.contains(&escaped_description), "{}", job.name());
        }
        assert!(body.contains("data-confirm"), "generate needs confirmation");
        assert!(body.contains("type=\"date\""), "the dated generate input");
        assert!(body.contains("pruned 0 embeddings"), "{body}");
        assert!(body.contains(&format!("/dashboard/jobs/{id}")), "{body}");
        assert!(!body.contains("style=\""), "no inline styles under the CSP");
    }

    #[tokio::test]
    async fn starting_a_job_inserts_a_row_and_starts_the_unit() {
        let runner = Arc::new(MockRunner::default());
        let (_dir, db, app) = app_with_runner(
            Config::default(),
            runner.clone() as Arc<dyn crate::web::JobRunner>,
        )
        .await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        let response = post(&app, "/dashboard/jobs/features-prune", "", &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let target = location(&response);
        assert!(target.starts_with("/dashboard/jobs/"), "{target}");
        let row = jobs::list(&db, 10).await.unwrap().remove(0);
        assert_eq!(target, format!("/dashboard/jobs/{}", row.id));
        assert_eq!(row.name, "features-prune");
        assert_eq!(row.unit, "daily-epub-job@features-prune.service");
        assert_eq!(row.status, "requested");
        assert_eq!(row.requested_by_name.as_deref(), Some("admin"));
        assert_eq!(
            runner.calls(),
            vec!["start daily-epub-job@features-prune.service".to_string()]
        );

        // The job page shows the row, the unit and refreshes while active.
        let page = get(&app, &target, Some(&admin)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let body = response_text(page).await;
        assert!(
            body.contains("daily-epub-job@features-prune.service"),
            "{body}"
        );
        assert!(body.contains("data-refresh=\"5\""), "{body}");
        assert!(body.contains("badge requested"), "{body}");
        assert!(body.contains("Started features-prune."), "the flash");

        // Simulate the unit claiming the request, then prove a running
        // duplicate is refused with 409 and no second row.
        assert_eq!(
            jobs::claim(&db, &Job::FeaturesPrune, Timestamp::now())
                .await
                .unwrap(),
            row.id
        );
        let duplicate = post(&app, "/dashboard/jobs/features-prune", "", &admin).await;
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let body = response_text(duplicate).await;
        assert!(body.contains("already requested or running"), "{body}");
        assert_eq!(jobs::list(&db, 10).await.unwrap().len(), 1);
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|call| call.starts_with("start "))
                .count(),
            1,
            "no second start"
        );

        // The dated generate form starts the dated unit.
        let dated = post(&app, "/dashboard/jobs/generate", "date=2026-09-03", &admin).await;
        assert_eq!(dated.status(), StatusCode::SEE_OTHER);
        let row = jobs::list(&db, 10).await.unwrap().remove(0);
        assert_eq!(row.name, "generate-2026-09-03");
        assert_eq!(
            runner.calls().last().unwrap(),
            "start daily-epub-job@generate-2026-09-03.service"
        );
        let bad_date = post(&app, "/dashboard/jobs/generate", "date=soon", &admin).await;
        assert_eq!(bad_date.status(), StatusCode::BAD_REQUEST);

        // Unknown names never reach the runner.
        for name in ["../x", "Generate", "backup"] {
            let unknown = post(&app, &format!("/dashboard/jobs/{name}"), "", &admin).await;
            assert_eq!(unknown.status(), StatusCode::NOT_FOUND, "{name}");
        }
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|call| call.starts_with("start "))
                .count(),
            2
        );

        // Readers cannot start jobs.
        let reader = login_cookie(&app, "reader", "correct horse battery").await;
        let forbidden = post(&app, "/dashboard/jobs/features-prune", "", &reader).await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_failed_start_marks_the_row_failed() {
        let runner = Arc::new(MockRunner::default());
        runner.fail_starts(Some("polkit: access denied"));
        let (_dir, db, app) = app_with_runner(
            Config::default(),
            runner.clone() as Arc<dyn crate::web::JobRunner>,
        )
        .await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let response = post(&app, "/dashboard/jobs/dry-run", "", &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let row = jobs::list(&db, 10).await.unwrap().remove(0);
        assert_eq!(row.status, "failed");
        assert!(row.finished_at.is_some());
        assert_eq!(
            row.message.as_deref(),
            Some("could not start daily-epub-job@dry-run.service: polkit: access denied")
        );
        let body = response_text(get(&app, &location(&response), Some(&admin)).await).await;
        assert!(body.contains("polkit: access denied"), "{body}");
        assert!(
            !body.contains("data-refresh"),
            "a finished job does not refresh"
        );
        // The unit is free again.
        runner.fail_starts(None);
        let again = post(&app, "/dashboard/jobs/dry-run", "", &admin).await;
        assert_eq!(again.status(), StatusCode::SEE_OTHER);
        assert_eq!(jobs::list(&db, 10).await.unwrap()[0].status, "requested");
    }

    #[tokio::test]
    async fn job_page_marks_a_unit_that_exited_before_the_job_started() {
        let runner = Arc::new(MockRunner::default());
        let unit = Job::ProfileRebuild.unit();
        runner.set_status(
            &unit,
            UnitStatus {
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                result: "exit-code".into(),
                exit_status: Some(1),
                started: Some("Thu 2026-09-03 05:30:01 EDT".into()),
                exited: Some("Thu 2026-09-03 05:30:02 EDT".into()),
            },
        );
        runner.set_log("Sep 03 05:30:01 daily-epub[1]: Error: profile rebuild is already running");
        let (_dir, db, app) = app_with_runner(
            Config::default(),
            runner.clone() as Arc<dyn crate::web::JobRunner>,
        )
        .await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;

        // Requested a minute ago and never claimed.
        let requested_at = Timestamp::now() - jiff::Span::new().seconds(60);
        let id = jobs::insert_requested(&db, &Job::ProfileRebuild, None, requested_at)
            .await
            .unwrap();
        let body =
            response_text(get(&app, &format!("/dashboard/jobs/{id}"), Some(&admin)).await).await;
        assert!(body.contains(EXITED_BEFORE_START), "{body}");
        assert!(body.contains("badge failed"), "{body}");
        assert!(body.contains("exit-code"), "the live unit status");
        assert!(body.contains("is already running"), "the journal tail");
        assert_eq!(jobs::get(&db, id).await.unwrap().unwrap().status, "failed");
        assert!(runner.calls().contains(&format!(
            "log {unit} {}",
            Config::default().server.journal_lines
        )));

        // A fresh request whose unit has not run yet is left alone.
        runner.set_status(
            &Job::FeaturesPrune.unit(),
            UnitStatus {
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                result: "success".into(),
                ..UnitStatus::default()
            },
        );
        let fresh = jobs::insert_requested(&db, &Job::FeaturesPrune, None, Timestamp::now())
            .await
            .unwrap();
        let body =
            response_text(get(&app, &format!("/dashboard/jobs/{fresh}"), Some(&admin)).await).await;
        assert!(body.contains("badge requested"), "{body}");
        assert_eq!(
            jobs::get(&db, fresh).await.unwrap().unwrap().status,
            "requested"
        );

        let missing = get(&app, "/dashboard/jobs/999999", Some(&admin)).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn disabled_jobs_refuse_starts_without_a_row() {
        let mut config = Config::default();
        config.server.jobs_enabled = false;
        let (_dir, db, app) = app_with_runner(config, Arc::new(crate::web::DisabledRunner)).await;
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let body = response_text(get(&app, "/dashboard/jobs", Some(&admin)).await).await;
        assert!(body.contains("Jobs are disabled"), "{body}");
        let response = post(&app, "/dashboard/jobs/features-prune", "", &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/dashboard/jobs");
        assert!(jobs::list(&db, 10).await.unwrap().is_empty());
    }
}
