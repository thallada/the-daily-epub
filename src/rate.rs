//! Shared construction of explicit rating events.

use crate::config::Config;
use crate::db::{Db, DbError};
use crate::types::{ArticleId, RatingEvent, Vote};

/// Append an explicit rating event from a named source.
pub async fn record_explicit(
    config: &Config,
    db: &Db,
    article_id: ArticleId,
    vote: Option<Vote>,
    source: &str,
    user_id: Option<i64>,
    note: Option<String>,
) -> Result<i64, DbError> {
    let (label, value) = match vote {
        Some(Vote::Loved) => ("loved", Vote::Loved.value(&config.curation.feedback)),
        Some(Vote::Good) => ("good", Vote::Good.value(&config.curation.feedback)),
        Some(Vote::NotForMe) => (
            "not_for_me",
            Vote::NotForMe.value(&config.curation.feedback),
        ),
        None => ("cleared", 0.0),
    };
    db.append_rating_event(&RatingEvent {
        id: 0,
        user_id,
        article_id,
        issue_date: db.latest_issue_date_for_article(article_id).await?,
        kind: "explicit".into(),
        source: source.into(),
        label: label.into(),
        value,
        note,
        event_at: jiff::Timestamp::now(),
    })
    .await
}
