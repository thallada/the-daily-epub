//! Reddit via the public JSON endpoints (spec §3.4, §3.7).
//!
//! Requires the descriptive User-Agent from [`crate::http::USER_AGENT`], ~1 req/s
//! pacing, and must degrade gracefully on 429 (social data is best-effort).

use jiff::Timestamp;
use serde::Deserialize;

use super::SocialError;
use crate::types::{ArticleId, Comment, CommentThread, SocialRef, SocialSource};

/// `GET /api/info.json?url=…` — finds submissions of a given URL (§3.4).
pub const INFO_URL: &str = "https://www.reddit.com/api/info.json";
pub const BASE_URL: &str = "https://www.reddit.com";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Listing {
    #[serde(default)]
    data: ListingData,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ListingData {
    #[serde(default)]
    children: Vec<Child>,
}

#[derive(Debug, Clone, Deserialize)]
struct Child {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// A `t3` submission (only the fields §3.4 uses).
#[derive(Debug, Clone, Deserialize)]
pub struct Post {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub score: Option<i64>,
    #[serde(default)]
    pub num_comments: Option<i64>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub subreddit: Option<String>,
}

impl Post {
    /// Absolute link a human can open (§3.4).
    pub fn item_url(&self) -> Option<String> {
        self.permalink.as_ref().map(|p| format!("{BASE_URL}{p}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawComment {
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    stickied: bool,
    #[serde(default)]
    replies: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Pure parsing (unit-tested against `tests/fixtures/reddit_*.json`)
// ---------------------------------------------------------------------------

/// Every `t3` submission in an `api/info.json` response.
pub fn parse_posts(body: &str) -> Result<Vec<Post>, SocialError> {
    let listing: Listing = serde_json::from_str(body).map_err(|e| SocialError::Unexpected {
        platform: "reddit",
        detail: e.to_string(),
    })?;
    Ok(listing
        .data
        .children
        .into_iter()
        .filter(|c| c.kind == "t3")
        .filter_map(|c| serde_json::from_value::<Post>(c.data).ok())
        .collect())
}

/// Best (highest score) submission in an `api/info.json` response (§3.4).
pub fn parse_info_response(
    body: &str,
    article_id: ArticleId,
    fetched_at: Timestamp,
) -> Result<Option<SocialRef>, SocialError> {
    let posts = parse_posts(body)?;
    let Some(best) = posts.into_iter().max_by_key(|p| p.score.unwrap_or(0)) else {
        return Ok(None);
    };
    Ok(Some(SocialRef {
        article_id,
        source: SocialSource::Reddit,
        item_id: best.name.clone().or_else(|| best.id.clone()),
        score: best.score.unwrap_or(0),
        num_comments: best.num_comments.unwrap_or(0),
        item_url: best.item_url(),
        fetched_at,
    }))
}

/// Parse a `{permalink}.json` body — `[post listing, comment listing]` (§3.7).
pub fn parse_comments_response(body: &str, permalink: &str) -> Result<CommentThread, SocialError> {
    let listings: Vec<Listing> =
        serde_json::from_str(body).map_err(|e| SocialError::Unexpected {
            platform: "reddit",
            detail: e.to_string(),
        })?;
    let total = listings
        .first()
        .and_then(|l| l.data.children.first())
        .and_then(|c| serde_json::from_value::<Post>(c.data.clone()).ok())
        .and_then(|p| p.num_comments)
        .unwrap_or(0);
    let comments = listings
        .get(1)
        .map(|l| map_children(&l.data.children, 0))
        .unwrap_or_default();
    Ok(CommentThread {
        source: SocialSource::Reddit,
        item_url: format!("{BASE_URL}{permalink}"),
        total_comments: total,
        comments,
    })
}

fn map_children(children: &[Child], depth: usize) -> Vec<Comment> {
    children
        .iter()
        .filter(|c| c.kind == "t1")
        .filter_map(|c| serde_json::from_value::<RawComment>(c.data.clone()).ok())
        .filter(|c| !c.stickied)
        .filter_map(|raw| {
            let body = raw.body.as_deref().unwrap_or("").trim().to_string();
            if body.is_empty() || body == "[removed]" || body == "[deleted]" {
                return None;
            }
            let kids = match serde_json::from_value::<Listing>(raw.replies.clone()) {
                Ok(listing) => map_children(&listing.data.children, depth + 1),
                // `replies` is `""` when a comment has none.
                Err(_) => Vec::new(),
            };
            Some(Comment {
                author: raw.author.clone().unwrap_or_else(|| "[deleted]".into()),
                points: raw.score,
                text_html: crate::extract::sanitize(&markdown_to_html(&body)),
                depth,
                children: kids,
            })
        })
        .collect()
}

/// Reddit comment bodies are markdown; the EPUB only needs paragraphs (§3.7).
fn markdown_to_html(body: &str) -> String {
    body.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", escape_text(p)))
        .collect()
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn get_text(
    http: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<String, SocialError> {
    let response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::http::USER_AGENT)
        .query(query)
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 403 {
        return Err(SocialError::RateLimited("reddit"));
    }
    Ok(response.error_for_status()?.text().await?)
}

/// Best (highest score) Reddit post for `canonical_url` (§3.4).
///
/// Returns `None` when no submission exists or the API rate-limited us.
pub async fn lookup_by_url(
    http: &reqwest::Client,
    canonical_url: &str,
    article_id: ArticleId,
) -> Result<Option<SocialRef>, SocialError> {
    let body = get_text(http, INFO_URL, &[("url", canonical_url), ("raw_json", "1")]).await?;
    parse_info_response(&body, article_id, Timestamp::now())
}

/// `GET {permalink}.json?limit=100&depth=3&sort=top` → comment tree (§3.7).
pub async fn fetch_comments(
    http: &reqwest::Client,
    permalink: &str,
) -> Result<CommentThread, SocialError> {
    let path = permalink.trim_end_matches('/');
    let url = format!("{BASE_URL}{path}.json");
    let body = get_text(
        http,
        &url,
        &[
            ("limit", "100"),
            ("depth", "3"),
            ("sort", "top"),
            ("raw_json", "1"),
        ],
    )
    .await?;
    parse_comments_response(&body, permalink)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = include_str!("../../tests/fixtures/m2_reddit_info.json");
    const INFO_EMPTY: &str = include_str!("../../tests/fixtures/m2_reddit_info_empty.json");
    const COMMENTS: &str = include_str!("../../tests/fixtures/m2_reddit_comments.json");

    fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z".parse().unwrap()
    }

    #[test]
    fn info_response_picks_the_highest_scoring_post() {
        let got = parse_info_response(INFO, 11, ts())
            .unwrap()
            .expect("a post");
        assert_eq!(got.article_id, 11);
        assert_eq!(got.source, SocialSource::Reddit);
        assert_eq!(got.item_id.as_deref(), Some("t3_1abcd2"));
        assert_eq!(got.score, 845);
        assert_eq!(got.num_comments, 231);
        assert_eq!(
            got.item_url.as_deref(),
            Some("https://www.reddit.com/r/programming/comments/1abcd2/a_deep_dive_into_btrees/")
        );
        assert_eq!(parse_posts(INFO).unwrap().len(), 3);
    }

    #[test]
    fn empty_info_response_is_not_an_error() {
        assert!(parse_info_response(INFO_EMPTY, 1, ts()).unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        assert!(matches!(
            parse_info_response("<html>rate limited</html>", 1, ts()).unwrap_err(),
            SocialError::Unexpected {
                platform: "reddit",
                ..
            }
        ));
    }

    #[test]
    fn comment_listing_becomes_a_tree() {
        let thread =
            parse_comments_response(COMMENTS, "/r/programming/comments/1abcd2/x/").unwrap();
        assert_eq!(thread.source, SocialSource::Reddit);
        assert_eq!(thread.total_comments, 231);
        assert_eq!(
            thread.item_url,
            "https://www.reddit.com/r/programming/comments/1abcd2/x/"
        );
        // `more` stubs, the stickied automod post and the removed comment are dropped.
        assert_eq!(thread.comments.len(), 1);

        let top = &thread.comments[0];
        assert_eq!(top.author, "index_nerd");
        assert_eq!(top.points, Some(412));
        assert_eq!(top.depth, 0);
        assert!(
            top.text_html
                .starts_with("<p>Fan-out is the whole ballgame")
        );
        assert_eq!(top.children.len(), 1);
        assert_eq!(top.children[0].author, "pagecache");
        assert_eq!(top.children[0].depth, 1);
    }

    #[test]
    fn comment_bodies_are_escaped_into_paragraphs() {
        let html = markdown_to_html("first & <b>bold</b>\n\nsecond");
        assert_eq!(
            html,
            "<p>first &amp; &lt;b&gt;bold&lt;/b&gt;</p><p>second</p>"
        );
        assert!(!crate::extract::sanitize(&html).contains("<b>"));
    }
}
