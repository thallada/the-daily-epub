//! The operator dashboard (`/dashboard/*`, dashboard plan §9–§14).
//!
//! Every route here sits under the admin `permission_required!` layer that
//! `web::router` applies to the merged router; handlers can therefore trust
//! that `AuthSession::user()` is an admin. One submodule per page group; each
//! exposes `routes()` and this module merges them.

pub mod articles;
pub mod jobs;
pub mod profile;
pub mod ratings;
pub mod runs;
pub mod settings;
pub mod stats;
pub mod users;

use askama::Template;
use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError};

/// Every dashboard route, without the admin layer (applied by the caller).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(overview))
        .merge(runs::routes())
        .merge(articles::routes())
        .merge(ratings::routes())
        .merge(profile::routes())
        .merge(settings::routes())
        .merge(jobs::routes())
        .merge(stats::routes())
        .merge(users::routes())
}

#[derive(Template)]
#[template(path = "dashboard/overview.html")]
struct OverviewTemplate {
    page: Page,
}

/// `GET /dashboard` — the overview (§9.1). Step 3 fills this in.
async fn overview(auth: AuthSession) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    Ok(Html(OverviewTemplate {
        page: Page::new("Overview", viewer, "dashboard"),
    })
    .into_response())
}
