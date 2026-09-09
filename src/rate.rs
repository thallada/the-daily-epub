//! Shared construction of explicit rating events.

/// The confirmation shown after an *AI slop* verdict: whether the author
/// penalty can apply depends on the article having an author at all (§9.3).
pub fn slop_message(author: Option<&str>) -> String {
    match author.map(str::trim).filter(|author| !author.is_empty()) {
        Some(author) => {
            format!("Recorded: AI slop — thanks. Future articles by {author} will rank much lower.")
        }
        None => "Recorded: AI slop — thanks. This article has no known author, so only the \
                 usual negative rating applies."
            .to_string(),
    }
}

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
        Some(vote) => (vote.event_label(), vote.value(&config.curation.feedback)),
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
