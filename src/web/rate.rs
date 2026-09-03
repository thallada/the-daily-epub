use std::collections::HashMap;

use axum::Form;
use axum::extract::{Extension, FromRequest, Json, Request, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum_login::tower_sessions::Session;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::server::AppState;
use crate::types::{ArticleId, RatingEvent, Vote};
use crate::web::session::{self, AuthSession};
use crate::web::{Flash, WebError};

#[derive(Debug, Clone)]
pub struct RatingWidget {
    pub article_id: ArticleId,
    pub issue_date: String,
    pub next: String,
    pub current: String,
    pub show_note: bool,
}

impl RatingWidget {
    pub fn for_issue(
        article_id: ArticleId,
        issue_date: Date,
        next: impl Into<String>,
        current: Option<&str>,
    ) -> Self {
        Self {
            article_id,
            issue_date: issue_date.to_string(),
            next: next.into(),
            current: web_label(current).to_string(),
            show_note: false,
        }
    }
}

fn web_label(label: Option<&str>) -> &str {
    match label {
        Some("not_for_me" | "down") => "down",
        Some("loved") => "loved",
        Some("good") => "good",
        Some("cleared") => "cleared",
        _ => "",
    }
}

pub async fn current_for_issue(
    state: &AppState,
    date: Date,
) -> Result<HashMap<ArticleId, String>, WebError> {
    let rows = sqlx::query(
        "WITH ranked AS (
             SELECT re.article_id, re.label,
                    ROW_NUMBER() OVER (
                        PARTITION BY re.article_id
                        ORDER BY re.event_at DESC, re.id DESC
                    ) AS event_rank
             FROM rating_events re
             JOIN issue_articles ia ON ia.article_id = re.article_id
             WHERE re.kind = 'explicit' AND ia.issue_date = ?
         )
         SELECT article_id, label FROM ranked WHERE event_rank = 1",
    )
    .bind(date.to_string())
    .fetch_all(state.db.pool())
    .await
    .map_err(crate::db::DbError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("article_id"), row.get("label")))
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct RatingInput {
    article_id: ArticleId,
    #[serde(default)]
    issue_date: Option<String>,
    label: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Serialize)]
struct RatingResponse {
    article_id: ArticleId,
    label: String,
    event_id: i64,
}

pub async fn post(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, WebError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let input = if content_type.starts_with("application/json") {
        Json::<RatingInput>::from_request(request, &state)
            .await
            .map(|Json(input)| input)
            .map_err(|_| WebError::BadRequest("invalid rating JSON".into()))?
    } else {
        Form::<RatingInput>::from_request(request, &state)
            .await
            .map(|Form(input)| input)
            .map_err(|_| WebError::BadRequest("invalid rating form".into()))?
    };

    let viewer = auth.user().await.ok_or_else(|| WebError::Unauthenticated {
        next: "/rate".into(),
    })?;
    if state.db.get_article(input.article_id).await?.is_none() {
        return Err(WebError::BadRequest(format!(
            "article {} does not exist",
            input.article_id
        )));
    }

    let issue_date = match input
        .issue_date
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(raw) => Some(
            raw.parse::<Date>()
                .map_err(|_| WebError::BadRequest("invalid issue date".into()))?,
        ),
        None => {
            state
                .db
                .latest_issue_date_for_article(input.article_id)
                .await?
        }
    };
    let config = state.config();
    let (event_label, value, response_label, flash_label) = match input.label.as_str() {
        "loved" => (
            "loved",
            Vote::Loved.value(&config.curation.feedback),
            "loved",
            "Loved it",
        ),
        "good" => (
            "good",
            Vote::Good.value(&config.curation.feedback),
            "good",
            "Good",
        ),
        "down" => (
            "not_for_me",
            Vote::NotForMe.value(&config.curation.feedback),
            "down",
            "Not for me",
        ),
        "cleared" => ("cleared", 0.0, "cleared", "Cleared"),
        _ => return Err(WebError::BadRequest("invalid rating label".into())),
    };
    let note = input
        .note
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());
    let event_id = state
        .db
        .append_rating_event(&RatingEvent {
            id: 0,
            user_id: Some(viewer.id),
            article_id: input.article_id,
            issue_date,
            kind: "explicit".into(),
            source: "dashboard".into(),
            label: event_label.into(),
            value,
            note,
            event_at: jiff::Timestamp::now(),
        })
        .await?;

    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    if wants_json {
        return Ok(Json(RatingResponse {
            article_id: input.article_id,
            label: response_label.into(),
            event_id,
        })
        .into_response());
    }

    session
        .insert(
            "flash",
            Flash {
                kind: "success".into(),
                text: format!("Rated: {flash_label}"),
            },
        )
        .await
        .map_err(|error| WebError::Internal(error.into()))?;
    let destination = session::valid_next(input.next.as_deref()).to_string();
    Ok(axum::response::Redirect::to(&destination).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_and_widget_labels_are_mapped_explicitly() {
        assert_eq!(web_label(Some("not_for_me")), "down");
        assert_eq!(web_label(Some("cleared")), "cleared");
        assert_eq!(web_label(None), "");
    }
}
