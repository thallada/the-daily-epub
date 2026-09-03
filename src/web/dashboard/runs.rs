//! Dashboard: runs pages. Filled in by web dashboard plan step 3.

use axum::Router;

use crate::server::AppState;

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new()
}
