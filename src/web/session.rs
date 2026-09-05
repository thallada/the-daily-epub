use std::collections::HashSet;
use std::str::FromStr;

use askama::Template;
use async_trait::async_trait;
use axum::Form;
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_login::tower_sessions::session::{Id, Record};
use axum_login::tower_sessions::{SessionStore, session_store};
use axum_login::{AuthUser, AuthnBackend, AuthzBackend};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

use crate::db::{Db, fmt_ts};
use crate::server::AppState;
use crate::web::users::{self, Role, User};
use crate::web::{Html, Page, WebError};

/// The session key axum-login keeps the signed-in user under (its default
/// `data_key`); the presence of this key is what "signed in" means to
/// [`crate::web::take_flash`]'s once-a-day session touch.
pub const AUTH_DATA_KEY: &str = "axum-login.data";

const DUMMY_HASH: &str = "$argon2i$v=19$m=65536,t=1,p=1$c29tZXNhbHQAAAAAAAAAAA$+r0d29hqEB0yasKr55ZgICsQGSkl0v0kgwhd+U3wyRo";

#[derive(Clone, Debug)]
pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn delete_expired(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM sessions WHERE expiry <= unixepoch()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_for_user(&self, user_id: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

fn store_error(error: impl std::fmt::Display) -> session_store::Error {
    session_store::Error::Backend(error.to_string())
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    /// Insert a brand-new record, drawing a fresh id on the (astronomically
    /// unlikely) collision with an existing row instead of overwriting it.
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        loop {
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE id = ?")
                .bind(record.id.to_string())
                .fetch_one(&self.pool)
                .await
                .map_err(store_error)?;
            if exists == 0 {
                return self.save(record).await;
            }
            record.id = Id::default();
        }
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let data = serde_json::to_string(&record.data).map_err(store_error)?;
        let user_id = record
            .data
            .get("axum-login.data")
            .and_then(|value| value.get("user_id"))
            .and_then(serde_json::Value::as_i64);
        let now = fmt_ts(jiff::Timestamp::now());
        sqlx::query(
            "INSERT INTO sessions (id, data, expiry, user_id, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, expiry = excluded.expiry,
                 user_id = excluded.user_id, updated_at = excluded.updated_at",
        )
        .bind(record.id.to_string())
        .bind(data)
        .bind(record.expiry_date.unix_timestamp())
        .bind(user_id)
        .bind(&now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>> {
        let row =
            sqlx::query("SELECT data, expiry FROM sessions WHERE id = ? AND expiry > unixepoch()")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(store_error)?;
        row.map(|row| {
            let data = serde_json::from_str(&row.get::<String, _>("data")).map_err(store_error)?;
            let expiry_date =
                OffsetDateTime::from_unix_timestamp(row.get("expiry")).map_err(store_error)?;
            Ok(Record {
                id: *id,
                data,
                expiry_date,
            })
        })
        .transpose()
    }

    async fn delete(&self, id: &Id) -> session_store::Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error("password verification task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub struct Backend {
    db: Db,
}

impl Backend {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl AuthUser for User {
    type Id = i64;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn session_auth_hash(&self) -> &[u8] {
        self.password_hash.as_bytes()
    }
}

impl AuthnBackend for Backend {
    type User = User;
    type Credentials = Credentials;
    type Error = BackendError;

    async fn authenticate(&self, creds: Credentials) -> Result<Option<User>, BackendError> {
        let user = users::find_by_username(&self.db, &creds.username).await?;
        let hash = user
            .as_ref()
            .map(|user| user.password_hash.clone())
            .unwrap_or_else(|| DUMMY_HASH.to_string());
        let password = creds.password;
        let valid =
            tokio::task::spawn_blocking(move || users::verify_password(&hash, &password)).await?;
        Ok(user.filter(|user| valid && !user.disabled))
    }

    async fn get_user(&self, id: &i64) -> Result<Option<User>, BackendError> {
        Ok(users::find_by_id(&self.db, *id)
            .await?
            .filter(|user| !user.disabled))
    }
}

impl AuthzBackend for Backend {
    type Permission = Role;

    async fn get_user_permissions(&self, user: &User) -> Result<HashSet<Role>, BackendError> {
        let permissions = match user.role {
            Role::Admin => [Role::User, Role::Admin].into_iter().collect(),
            Role::User => [Role::User].into_iter().collect(),
        };
        Ok(permissions)
    }
}

pub type AuthSession = axum_login::AuthSession<Backend>;

#[derive(Debug, Clone)]
pub struct Viewer {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

impl From<User> for Viewer {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            username: user.username,
            role: user.role,
        }
    }
}

pub fn valid_next(next: Option<&str>) -> &str {
    next.filter(|next| next.starts_with('/') && !next.starts_with("//"))
        .unwrap_or("/")
}

pub async fn require_same_origin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let headers = request.headers();
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if !matches!(site, "same-origin" | "none") {
            return StatusCode::FORBIDDEN.into_response();
        }
    } else if let Some(origin) = request_origin(headers) {
        let config = state.config();
        let public_origin = url::Url::parse(&config.server.public_url)
            .ok()
            .map(|url| url.origin().ascii_serialization());
        let host_origin = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(|host| format!("{}://{host}", forwarded_scheme(headers)));
        if public_origin.as_deref() != Some(origin.as_str())
            && host_origin.as_deref() != Some(origin.as_str())
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

#[derive(Debug, Default, Deserialize)]
pub struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    page: Page,
    next: String,
    error: String,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountTemplate {
    page: Page,
    error: String,
}

/// The `<meta name="description">` for both renders of the sign-in page.
const LOGIN_DESCRIPTION: &str = concat!(
    "Sign in to The Daily EPUB to read the full issue, ",
    "rate what you read, and download the morning's editions."
);

pub async fn login_page(auth: AuthSession, Query(query): Query<LoginQuery>) -> Response {
    let viewer = auth.user().await.map(Viewer::from);
    Html(LoginTemplate {
        page: Page::new("Sign in", viewer, "login").with_description(LOGIN_DESCRIPTION),
        next: valid_next(query.next.as_deref()).to_string(),
        error: String::new(),
    })
    .into_response()
}

pub async fn login(
    State(state): State<AppState>,
    auth: AuthSession,
    Form(credentials): Form<Credentials>,
) -> Result<Response, WebError> {
    let destination = valid_next(credentials.next.as_deref()).to_string();
    match auth
        .authenticate(credentials)
        .await
        .map_err(|error| WebError::Internal(error.into()))?
    {
        Some(user) => {
            auth.login(&user)
                .await
                .map_err(|error| WebError::Internal(error.into()))?;
            sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
                .bind(fmt_ts(jiff::Timestamp::now()))
                .bind(user.id)
                .execute(state.db.pool())
                .await
                .map_err(crate::db::DbError::from)?;
            Ok(axum::response::Redirect::to(&destination).into_response())
        }
        None => Ok((
            StatusCode::UNAUTHORIZED,
            Html(LoginTemplate {
                page: Page::new("Sign in", None, "login").with_description(LOGIN_DESCRIPTION),
                next: destination,
                error: "invalid username or password".into(),
            }),
        )
            .into_response()),
    }
}

pub async fn logout(auth: AuthSession) -> Result<Response, WebError> {
    auth.logout()
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    Ok(axum::response::Redirect::to("/").into_response())
}

pub async fn account(auth: AuthSession) -> Result<Response, WebError> {
    let user = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/account".into(),
    })?;
    Ok(Html(AccountTemplate {
        page: Page::new("Account", Some(user.into()), "account"),
        error: String::new(),
    })
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct PasswordForm {
    current_password: String,
    new_password: String,
    confirm_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    auth: AuthSession,
    Form(form): Form<PasswordForm>,
) -> Result<Response, WebError> {
    let user = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/account".into(),
    })?;
    let hash = user.password_hash.clone();
    let current = form.current_password;
    let valid = tokio::task::spawn_blocking(move || users::verify_password(&hash, &current))
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    let problem = if !valid {
        Some("current password is incorrect".to_string())
    } else if form.new_password != form.confirm_password {
        Some("the new passwords do not match".to_string())
    } else {
        users::validate_password(&form.new_password)
            .err()
            .map(|error| error.to_string())
    };
    if let Some(error) = problem {
        return Ok((
            StatusCode::BAD_REQUEST,
            Html(AccountTemplate {
                page: Page::new("Account", Some(user.into()), "account"),
                error,
            }),
        )
            .into_response());
    }
    let password = form.new_password;
    let password_hash = tokio::task::spawn_blocking(move || users::hash_password(&password))
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&password_hash)
        .bind(user.id)
        .execute(state.db.pool())
        .await
        .map_err(crate::db::DbError::from)?;
    SqliteSessionStore::new(state.db.pool().clone())
        .delete_for_user(user.id)
        .await
        .map_err(crate::db::DbError::from)?;
    let mut updated = user;
    updated.password_hash = password_hash;
    auth.login(&updated)
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    Ok(axum::response::Redirect::to("/account").into_response())
}

pub async fn logout_everywhere(
    State(state): State<AppState>,
    auth: AuthSession,
) -> Result<Response, WebError> {
    let user = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/account".into(),
    })?;
    SqliteSessionStore::new(state.db.pool().clone())
        .delete_for_user(user.id)
        .await
        .map_err(crate::db::DbError::from)?;
    auth.logout()
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    Ok(axum::response::Redirect::to("/").into_response())
}

fn forwarded_scheme(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http")
}

fn request_origin(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        return Some(origin.trim_end_matches('/').to_string());
    }
    let referer = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())?;
    let url = url::Url::from_str(referer).ok()?;
    Some(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum_login::tower_sessions::SessionStore;
    use serde_json::json;
    use time::Duration;

    use super::*;

    async fn store() -> (tempfile::TempDir, Db, SqliteSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let store = SqliteSessionStore::new(db.pool().clone());
        (dir, db, store)
    }

    fn record(user_id: Option<i64>, expiry: OffsetDateTime) -> Record {
        let mut data = HashMap::new();
        if let Some(user_id) = user_id {
            data.insert(
                "axum-login.data".into(),
                json!({"user_id": user_id, "auth_hash": [1, 2, 3]}),
            );
        }
        Record {
            id: Id::default(),
            data,
            expiry_date: expiry,
        }
    }

    #[tokio::test]
    async fn session_store_round_trips_denormalizes_and_deletes() {
        let (_dir, db, store) = store().await;
        let user = users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let active = record(
            Some(user.id),
            OffsetDateTime::now_utc() + Duration::hours(1),
        );
        store.save(&active).await.unwrap();
        let loaded = store.load(&active.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, active.id);
        assert_eq!(loaded.data, active.data);
        assert_eq!(
            loaded.expiry_date.unix_timestamp(),
            active.expiry_date.unix_timestamp()
        );
        let denormalized: Option<i64> =
            sqlx::query_scalar("SELECT user_id FROM sessions WHERE id = ?")
                .bind(active.id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(denormalized, Some(user.id));
        assert_eq!(store.delete_for_user(user.id).await.unwrap(), 1);
        assert!(store.load(&active.id).await.unwrap().is_none());

        let anonymous = record(None, OffsetDateTime::now_utc() + Duration::hours(1));
        store.save(&anonymous).await.unwrap();
        let denormalized: Option<i64> =
            sqlx::query_scalar("SELECT user_id FROM sessions WHERE id = ?")
                .bind(anonymous.id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(denormalized, None);
        store.delete(&anonymous.id).await.unwrap();
        assert!(store.load(&anonymous.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expired_sessions_are_hidden_and_pruned_and_errors_surface() {
        let (_dir, db, store) = store().await;
        let expired = record(None, OffsetDateTime::now_utc() - Duration::seconds(1));
        store.save(&expired).await.unwrap();
        assert!(store.load(&expired.id).await.unwrap().is_none());
        assert_eq!(store.delete_expired().await.unwrap(), 1);
        db.close().await;
        let error = store.save(&record(None, OffsetDateTime::now_utc())).await;
        assert!(matches!(error, Err(session_store::Error::Backend(_))));
    }

    #[test]
    fn next_targets_must_be_same_site_paths() {
        assert_eq!(valid_next(Some("/dashboard")), "/dashboard");
        assert_eq!(valid_next(Some("//evil.example/")), "/");
        assert_eq!(valid_next(Some("https://evil.example/")), "/");
    }
}
