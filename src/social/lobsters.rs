//! Lobsters via `/s/{id}.json` (spec §3.4, §3.7).
//!
//! Lobsters has no public URL-search API, so linkage only works when the entry
//! arrived through a lobste.rs feed or its `comments_url` points at a story (§7).

use jiff::Timestamp;
use serde::Deserialize;
use url::Url;

use super::SocialError;
use crate::types::{ArticleId, Comment, CommentThread, SocialRef, SocialSource};

pub const STORY_URL_PREFIX: &str = "https://lobste.rs/s/";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Story {
    #[serde(default)]
    short_id: Option<String>,
    #[serde(default)]
    short_id_url: Option<String>,
    #[serde(default)]
    comments_url: Option<String>,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    comment_count: Option<i64>,
    #[serde(default)]
    comments: Vec<RawComment>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawComment {
    #[serde(default)]
    short_id: Option<String>,
    #[serde(default)]
    parent_comment: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    is_deleted: bool,
    /// String in the current API; older responses nested it in an object.
    #[serde(default)]
    commenting_user: Option<serde_json::Value>,
}

impl RawComment {
    fn author(&self) -> String {
        match &self.commenting_user {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Object(o)) => o
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("[unknown]")
                .to_string(),
            _ => "[unknown]".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure parsing (unit-tested against `tests/fixtures/m2_lobsters_story.json`)
// ---------------------------------------------------------------------------

/// Extract the story id from a `lobste.rs/s/<id>` URL (§3.4).
pub fn story_id_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "lobste.rs" && !host.ends_with(".lobste.rs") {
        return None;
    }
    let mut segments = parsed.path_segments()?;
    if segments.next()? != "s" {
        return None;
    }
    segments
        .next()
        .map(str::to_string)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn parse_story(body: &str) -> Result<Story, SocialError> {
    serde_json::from_str(body).map_err(|e| SocialError::Unexpected {
        platform: "lobsters",
        detail: e.to_string(),
    })
}

/// Turn a `/s/{id}.json` body into a [`SocialRef`] (§3.4).
pub fn parse_story_response(
    body: &str,
    story_id: &str,
    article_id: ArticleId,
    fetched_at: Timestamp,
) -> Result<Option<SocialRef>, SocialError> {
    let story = parse_story(body)?;
    let id = story
        .short_id
        .clone()
        .unwrap_or_else(|| story_id.to_string());
    Ok(Some(SocialRef {
        article_id,
        source: SocialSource::Lobsters,
        item_id: Some(id.clone()),
        score: story.score.unwrap_or(0),
        num_comments: story.comment_count.unwrap_or(story.comments.len() as i64),
        item_url: Some(
            story
                .short_id_url
                .or(story.comments_url)
                .unwrap_or_else(|| format!("{STORY_URL_PREFIX}{id}")),
        ),
        fetched_at,
    }))
}

/// Turn the same body's flat `comments` array into a nested tree (§3.7).
pub fn parse_comments_response(body: &str, story_id: &str) -> Result<CommentThread, SocialError> {
    let story = parse_story(body)?;
    let id = story
        .short_id
        .clone()
        .unwrap_or_else(|| story_id.to_string());
    let item_url = story
        .short_id_url
        .clone()
        .or_else(|| story.comments_url.clone())
        .unwrap_or_else(|| format!("{STORY_URL_PREFIX}{id}"));
    let total = story.comment_count.unwrap_or(story.comments.len() as i64);
    Ok(CommentThread {
        source: SocialSource::Lobsters,
        item_url,
        total_comments: total,
        comments: build_tree(&story.comments),
    })
}

/// Lobsters returns a flat list ordered depth-first with `parent_comment` links.
fn build_tree(raw: &[RawComment]) -> Vec<Comment> {
    let mut roots: Vec<Comment> = Vec::new();
    // Path of `short_id`s from the root to the comment most recently inserted.
    let mut path: Vec<String> = Vec::new();

    for item in raw {
        if item.is_deleted {
            continue;
        }
        let text = item.comment.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let comment = Comment {
            author: item.author(),
            points: item.score,
            text_html: crate::extract::sanitize(text),
            depth: 0,
            children: Vec::new(),
        };
        let short_id = item.short_id.clone().unwrap_or_default();
        match item.parent_comment.as_deref() {
            Some(parent) => {
                while path.last().is_some_and(|p| p != parent) {
                    path.pop();
                }
                if path.is_empty() {
                    // Parent was dropped (deleted); promote to a root thread.
                    roots.push(comment);
                    path = vec![short_id];
                    continue;
                }
                let depth = path.len();
                if let Some(node) = descend(&mut roots, &path) {
                    let mut child = comment;
                    child.depth = depth;
                    node.children.push(child);
                    path.push(short_id);
                }
            }
            None => {
                roots.push(comment);
                path = vec![short_id];
            }
        }
    }
    roots
}

/// Walk `roots` along the ids in `path`, returning the last node on it.
fn descend<'a>(roots: &'a mut [Comment], path: &[String]) -> Option<&'a mut Comment> {
    let mut node = roots.last_mut()?;
    for _ in 1..path.len() {
        node = node.children.last_mut()?;
    }
    Some(node)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn get_text(http: &reqwest::Client, story_id: &str) -> Result<String, SocialError> {
    let url = format!("{STORY_URL_PREFIX}{story_id}.json");
    let response = http.get(&url).send().await?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(SocialError::RateLimited("lobsters"));
    }
    Ok(response.error_for_status()?.text().await?)
}

/// `GET https://lobste.rs/s/{id}.json` → score + comment count (§3.4).
pub async fn fetch_story(
    http: &reqwest::Client,
    story_id: &str,
    article_id: ArticleId,
) -> Result<Option<SocialRef>, SocialError> {
    let body = get_text(http, story_id).await?;
    parse_story_response(&body, story_id, article_id, Timestamp::now())
}

/// The same endpoint's `comments` array, as a tree (§3.7).
pub async fn fetch_comments(
    http: &reqwest::Client,
    story_id: &str,
) -> Result<CommentThread, SocialError> {
    let body = get_text(http, story_id).await?;
    parse_comments_response(&body, story_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STORY: &str = include_str!("../../tests/fixtures/m2_lobsters_story.json");

    fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z".parse().unwrap()
    }

    #[test]
    fn story_ids_come_out_of_lobsters_urls() {
        assert_eq!(
            story_id_from_url("https://lobste.rs/s/abcdef/a_deep_dive").as_deref(),
            Some("abcdef")
        );
        assert_eq!(
            story_id_from_url("https://lobste.rs/s/abcdef").as_deref(),
            Some("abcdef")
        );
        assert_eq!(story_id_from_url("https://lobste.rs/"), None);
        assert_eq!(story_id_from_url("https://lobste.rs/s/"), None);
        assert_eq!(
            story_id_from_url("https://news.ycombinator.com/item?id=1"),
            None
        );
        assert_eq!(story_id_from_url("garbage"), None);
    }

    #[test]
    fn story_response_gives_score_and_comment_count() {
        let got = parse_story_response(STORY, "abcdef", 9, ts())
            .unwrap()
            .expect("a story");
        assert_eq!(got.article_id, 9);
        assert_eq!(got.source, SocialSource::Lobsters);
        assert_eq!(got.item_id.as_deref(), Some("abcdef"));
        assert_eq!(got.score, 78);
        assert_eq!(got.num_comments, 4);
        assert_eq!(got.item_url.as_deref(), Some("https://lobste.rs/s/abcdef"));
    }

    #[test]
    fn flat_comments_become_a_tree() {
        let thread = parse_comments_response(STORY, "abcdef").unwrap();
        assert_eq!(thread.source, SocialSource::Lobsters);
        assert_eq!(thread.total_comments, 4);
        // Two top-level threads; the deleted reply is dropped.
        assert_eq!(thread.comments.len(), 2);

        let first = &thread.comments[0];
        assert_eq!(first.author, "bob");
        assert_eq!(first.points, Some(21));
        assert_eq!(first.depth, 0);
        assert_eq!(first.children.len(), 1);
        assert_eq!(first.children[0].author, "carol");
        assert_eq!(first.children[0].depth, 1);
        assert!(first.children[0].text_html.contains("tight too"));

        let second = &thread.comments[1];
        assert_eq!(second.author, "dave");
        assert!(second.children.is_empty());
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        assert!(matches!(
            parse_story_response("nope", "abcdef", 1, ts()).unwrap_err(),
            SocialError::Unexpected {
                platform: "lobsters",
                ..
            }
        ));
    }
}
