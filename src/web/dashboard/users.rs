//! Dashboard: the read-only Users page (`/dashboard/users`, plan §6.1).

use askama::Template;
use axum::Router;
use axum::extract::{Extension, State};
use axum::routing::get;
use axum_login::tower_sessions::Session;

use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError, format_time, take_flash};

#[derive(Debug)]
struct UserLine {
    username: String,
    role: String,
    disabled: bool,
    created: String,
    last_login: String,
    open_sessions: i64,
}

#[derive(Template)]
#[template(path = "dashboard/users.html")]
struct UsersTemplate {
    page: Page,
    users: Vec<UserLine>,
}

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new().route("/dashboard/users", get(index))
}

async fn index(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
) -> Result<Html<UsersTemplate>, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let config = state.config();
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
    Ok(Html(UsersTemplate { page, users }))
}

#[cfg(test)]
mod tests {
    use crate::web::dashboard::tests::{app_with_users, assert_admin_only, seed};

    #[tokio::test]
    async fn users_page_is_admin_only_and_lists_accounts_and_sessions() {
        let seed = seed().await;
        let app = app_with_users(&seed.db).await;
        let body = assert_admin_only(&app, "/dashboard/users").await;

        assert!(body.contains("<h1>Users</h1>"), "{body}");
        assert!(body.contains("reader"), "{body}");
        assert!(body.contains("admin"), "{body}");
        assert!(body.contains("Open sessions"), "{body}");
        assert!(body.contains("daily-epub users"), "{body}");
    }
}
