use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use sqlx::Row;

use crate::db::{Db, DbError, fmt_ts, parse_ts};

pub const MIN_PASSWORD_LEN: usize = 12;
pub const MAX_PASSWORD_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(Self::User),
            "admin" => Ok(Self::Admin),
            _ => Err(format!("invalid role {value:?}; expected user or admin")),
        }
    }
}

#[derive(Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub disabled: bool,
    pub created_at: Timestamp,
    pub last_login_at: Option<Timestamp>,
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("password_hash", &"[REDACTED]")
            .field("role", &self.role)
            .field("disabled", &self.disabled)
            .field("created_at", &self.created_at)
            .field("last_login_at", &self.last_login_at)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct UserListRow {
    pub user: User,
    pub open_sessions: i64,
}

pub fn validate_username(username: &str) -> anyhow::Result<()> {
    if username.is_empty()
        || username.len() > 32
        || !username
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        anyhow::bail!("username must be 1-32 characters from A-Z, a-z, 0-9, '.', '_' or '-'");
    }
    Ok(())
}

pub fn validate_password(password: &str) -> anyhow::Result<()> {
    if password.len() < MIN_PASSWORD_LEN {
        anyhow::bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    if password.len() > MAX_PASSWORD_LEN {
        anyhow::bail!("password must be at most {MAX_PASSWORD_LEN} characters");
    }
    Ok(())
}

pub fn hash_password(plain: &str) -> String {
    password_auth::generate_hash(plain)
}

pub fn verify_password(hash: &str, plain: &str) -> bool {
    password_auth::verify_password(plain, hash).is_ok()
}

pub async fn find_by_username(db: &Db, username: &str) -> Result<Option<User>, DbError> {
    let row = sqlx::query(
        "SELECT id, username, password_hash, role, disabled, created_at, last_login_at
         FROM users WHERE username = ? COLLATE NOCASE",
    )
    .bind(username)
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(user_from_row).transpose()
}

pub async fn find_by_id(db: &Db, id: i64) -> Result<Option<User>, DbError> {
    let row = sqlx::query(
        "SELECT id, username, password_hash, role, disabled, created_at, last_login_at
         FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await?;
    row.as_ref().map(user_from_row).transpose()
}

pub async fn add(db: &Db, username: &str, password: &str, admin: bool) -> anyhow::Result<User> {
    validate_username(username)?;
    validate_password(password)?;
    if find_by_username(db, username).await?.is_some() {
        anyhow::bail!("user {username:?} already exists");
    }
    let hash = hash_password(password);
    let created_at = Timestamp::now();
    let role = if admin { Role::Admin } else { Role::User };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (username, password_hash, role, created_at)
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(username)
    .bind(&hash)
    .bind(role.as_str())
    .bind(fmt_ts(created_at))
    .fetch_one(db.pool())
    .await?;
    Ok(User {
        id,
        username: username.to_string(),
        password_hash: hash,
        role,
        disabled: false,
        created_at,
        last_login_at: None,
    })
}

pub async fn passwd(db: &Db, username: &str, password: &str) -> anyhow::Result<u64> {
    validate_password(password)?;
    let hash = hash_password(password);
    let result =
        sqlx::query("UPDATE users SET password_hash = ? WHERE username = ? COLLATE NOCASE")
            .bind(hash)
            .bind(username)
            .execute(db.pool())
            .await?;
    require_one(username, result.rows_affected())?;
    logout(db, username).await
}

pub async fn set_role(db: &Db, username: &str, role: Role) -> anyhow::Result<()> {
    let result = sqlx::query("UPDATE users SET role = ? WHERE username = ? COLLATE NOCASE")
        .bind(role.as_str())
        .bind(username)
        .execute(db.pool())
        .await?;
    require_one(username, result.rows_affected())
}

pub async fn set_disabled(db: &Db, username: &str, disabled: bool) -> anyhow::Result<u64> {
    let result = sqlx::query("UPDATE users SET disabled = ? WHERE username = ? COLLATE NOCASE")
        .bind(disabled)
        .bind(username)
        .execute(db.pool())
        .await?;
    require_one(username, result.rows_affected())?;
    if disabled {
        logout(db, username).await
    } else {
        Ok(0)
    }
}

pub async fn logout(db: &Db, username: &str) -> anyhow::Result<u64> {
    let Some(user) = find_by_username(db, username).await? else {
        anyhow::bail!("user {username:?} was not found");
    };
    let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user.id)
        .execute(db.pool())
        .await?;
    Ok(result.rows_affected())
}

pub async fn list(db: &Db) -> anyhow::Result<Vec<UserListRow>> {
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.password_hash, u.role, u.disabled, u.created_at,
                u.last_login_at, COUNT(s.id) AS open_sessions
         FROM users u LEFT JOIN sessions s ON s.user_id = u.id AND s.expiry > unixepoch()
         GROUP BY u.id ORDER BY u.username COLLATE NOCASE",
    )
    .fetch_all(db.pool())
    .await?;
    rows.iter()
        .map(|row| {
            Ok(UserListRow {
                user: user_from_row(row)?,
                open_sessions: row.get("open_sessions"),
            })
        })
        .collect()
}

fn require_one(username: &str, count: u64) -> anyhow::Result<()> {
    if count == 0 {
        anyhow::bail!("user {username:?} was not found");
    }
    Ok(())
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<User, DbError> {
    let role_raw: String = row.get("role");
    let role = role_raw.parse().map_err(|_| DbError::Decode {
        column: "users.role",
        value: role_raw,
    })?;
    Ok(User {
        id: row.get("id"),
        username: row.get("username"),
        password_hash: row.get("password_hash"),
        role,
        disabled: row.get("disabled"),
        created_at: parse_ts("users.created_at", &row.get::<String, _>("created_at"))?,
        last_login_at: row
            .get::<Option<String>, _>("last_login_at")
            .map(|value| parse_ts("users.last_login_at", &value))
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_round_trip_and_fail_safely() {
        let hash = hash_password("correct horse battery");
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "wrong password"));
        assert!(!verify_password("not a phc string", "anything"));
        assert!(
            !format!(
                "{:?}",
                User {
                    id: 1,
                    username: "reader".into(),
                    password_hash: hash.clone(),
                    role: Role::User,
                    disabled: false,
                    created_at: Timestamp::now(),
                    last_login_at: None,
                }
            )
            .contains(&hash)
        );
    }

    #[tokio::test]
    async fn user_operations_validate_and_are_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        assert!(add(&db, "reader", "too-short", false).await.is_err());
        let user = add(&db, "Reader", "correct horse battery", true)
            .await
            .unwrap();
        assert_eq!(user.role, Role::Admin);
        assert!(
            add(&db, "reader", "another valid password", false)
                .await
                .is_err()
        );
        set_role(&db, "READER", Role::User).await.unwrap();
        assert_eq!(
            find_by_username(&db, "reader").await.unwrap().unwrap().role,
            Role::User
        );
        passwd(&db, "reader", "a replacement password")
            .await
            .unwrap();
        set_disabled(&db, "reader", true).await.unwrap();
        assert!(
            find_by_username(&db, "reader")
                .await
                .unwrap()
                .unwrap()
                .disabled
        );
        set_disabled(&db, "reader", false).await.unwrap();
    }
}
