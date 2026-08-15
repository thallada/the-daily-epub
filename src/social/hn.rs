//! HackerNews via the Algolia API (spec §3.4, §3.7).

use jiff::Timestamp;
use serde::Deserialize;
use url::Url;

use super::SocialError;
use crate::types::{ArticleId, Comment, CommentThread, SocialRef, SocialSource};

/// Algolia search endpoint (free, generous limits) (§3.4).
pub const SEARCH_URL: &str = "https://hn.algolia.com/api/v1/search";
/// Algolia item-tree endpoint used for comment chapters (§3.7).
pub const ITEM_URL: &str = "https://hn.algolia.com/api/v1/items";
/// Canonical HN item page prefix.
pub const ITEM_PAGE: &str = "https://news.ycombinator.com/item?id=";

/// Hits requested per URL search — enough to spot the canonical submission.
const HITS_PER_PAGE: &str = "10";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One Algolia search hit (only the fields §3.4 uses).
#[derive(Debug, Clone, Deserialize)]
pub struct Hit {
    #[serde(rename = "objectID")]
    pub object_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub points: Option<i64>,
    #[serde(default)]
    pub num_comments: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<Hit>,
}

/// A node of the Algolia item tree (`/items/{id}`) (§3.7).
#[derive(Debug, Clone, Deserialize)]
struct Item {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    author: Option<String>,
    // The story's own `title` is deliberately not deserialized here: the comment
    // renderer takes the article title from the `Pick`, not from Algolia (§3.7).
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    children: Vec<Item>,
}

// ---------------------------------------------------------------------------
// Pure parsing (unit-tested against `tests/fixtures/hn_*.json`)
// ---------------------------------------------------------------------------

/// Extract the story id from a `news.ycombinator.com/item?id=N` comments URL (§3.4).
pub fn story_id_from_comments_url(comments_url: &str) -> Option<String> {
    let url = Url::parse(comments_url.trim()).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "news.ycombinator.com" && !host.ends_with(".ycombinator.com") {
        return None;
    }
    if !url.path().starts_with("/item") {
        return None;
    }
    url.query_pairs()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v.into_owned())
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
}

fn parse_hits(body: &str) -> Result<Vec<Hit>, SocialError> {
    serde_json::from_str::<SearchResponse>(body)
        .map(|r| r.hits)
        .map_err(|e| SocialError::Unexpected {
            platform: "hn",
            detail: e.to_string(),
        })
}

/// Turn a search response into the best [`SocialRef`] for `canonical_url` (§3.4).
///
/// Prefers a hit whose own URL canonicalizes to `canonical_url`; otherwise the
/// highest-scoring hit wins (Algolia sorts by relevance, not points).
pub fn parse_search_response(
    body: &str,
    canonical_url: &str,
    article_id: ArticleId,
    fetched_at: Timestamp,
) -> Result<Option<SocialRef>, SocialError> {
    let hits = parse_hits(body)?;
    Ok(best_hit(&hits, Some(canonical_url)).map(|hit| social_ref(hit, article_id, fetched_at)))
}

/// Turn a `search?tags=story_{id}` response into a [`SocialRef`] (§3.4).
pub fn parse_story_response(
    body: &str,
    article_id: ArticleId,
    fetched_at: Timestamp,
) -> Result<Option<SocialRef>, SocialError> {
    let hits = parse_hits(body)?;
    Ok(best_hit(&hits, None).map(|hit| social_ref(hit, article_id, fetched_at)))
}

fn best_hit<'a>(hits: &'a [Hit], canonical_url: Option<&str>) -> Option<&'a Hit> {
    let exact = canonical_url.and_then(|want| {
        hits.iter()
            .filter(|h| {
                h.url
                    .as_deref()
                    .and_then(crate::dedupe::canonical_url)
                    .is_some_and(|c| c == want)
            })
            .max_by_key(|h| h.points.unwrap_or(0))
    });
    exact.or_else(|| hits.iter().max_by_key(|h| h.points.unwrap_or(0)))
}

fn social_ref(hit: &Hit, article_id: ArticleId, fetched_at: Timestamp) -> SocialRef {
    SocialRef {
        article_id,
        source: SocialSource::Hn,
        item_id: Some(hit.object_id.clone()),
        score: hit.points.unwrap_or(0),
        num_comments: hit.num_comments.unwrap_or(0),
        item_url: Some(format!("{ITEM_PAGE}{}", hit.object_id)),
        fetched_at,
    }
}

/// Parse `/items/{id}` into a [`CommentThread`] (§3.7).
///
/// The tree is returned in full; [`crate::comments::truncate`] applies the §3.7
/// display limits.
pub fn parse_item_response(body: &str) -> Result<CommentThread, SocialError> {
    let item: Item = serde_json::from_str(body).map_err(|e| SocialError::Unexpected {
        platform: "hn",
        detail: e.to_string(),
    })?;
    let object_id = item.id.map(|i| i.to_string()).unwrap_or_default();
    let comments = map_children(&item.children, 0);
    let total = count_comments(&item.children);
    Ok(CommentThread {
        source: SocialSource::Hn,
        item_url: format!("{ITEM_PAGE}{object_id}"),
        total_comments: total,
        comments,
    })
}

fn map_children(children: &[Item], depth: usize) -> Vec<Comment> {
    children
        .iter()
        .filter(|c| c.kind.as_deref() != Some("story"))
        .filter_map(|child| {
            let text = child.text.as_deref().unwrap_or("").trim();
            let kids = map_children(&child.children, depth + 1);
            if text.is_empty() && kids.is_empty() {
                return None; // deleted comment with no surviving replies
            }
            Some(Comment {
                author: child.author.clone().unwrap_or_else(|| "[deleted]".into()),
                points: child.points,
                text_html: crate::extract::sanitize(text),
                depth,
                children: kids,
            })
        })
        .collect()
}

fn count_comments(children: &[Item]) -> i64 {
    children
        .iter()
        .map(|c| 1 + count_comments(&c.children))
        .sum()
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn get_text(
    http: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<String, SocialError> {
    let response = http.get(url).query(query).send().await?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(SocialError::RateLimited("hn"));
    }
    Ok(response.error_for_status()?.text().await?)
}

/// `GET /search?query=<url>&restrictSearchableAttributes=url` → best hit (§3.4).
///
/// Returns `None` when HN has no submission for this URL.
pub async fn search_by_url(
    http: &reqwest::Client,
    canonical_url: &str,
    article_id: ArticleId,
) -> Result<Option<SocialRef>, SocialError> {
    let body = get_text(
        http,
        SEARCH_URL,
        &[
            ("query", canonical_url),
            ("restrictSearchableAttributes", "url"),
            ("tags", "story"),
            ("hitsPerPage", HITS_PER_PAGE),
        ],
    )
    .await?;
    parse_search_response(&body, canonical_url, article_id, Timestamp::now())
}

/// `GET /search?tags=story_{id}` → points/comment count for a known story (§3.4).
///
/// The `/items/{id}` endpoint carries the whole comment tree; the search endpoint
/// answers the same question with a fraction of the bytes.
pub async fn fetch_story(
    http: &reqwest::Client,
    object_id: &str,
    article_id: ArticleId,
) -> Result<Option<SocialRef>, SocialError> {
    let tag = format!("story_{object_id}");
    let body = get_text(
        http,
        SEARCH_URL,
        &[("tags", tag.as_str()), ("hitsPerPage", "1")],
    )
    .await?;
    parse_story_response(&body, article_id, Timestamp::now())
}

/// Full comment tree for a story (§3.7).
pub async fn fetch_comments(
    http: &reqwest::Client,
    object_id: &str,
) -> Result<CommentThread, SocialError> {
    let url = format!("{ITEM_URL}/{object_id}");
    let body = get_text(http, &url, &[]).await?;
    parse_item_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = include_str!("../../tests/fixtures/m2_hn_search_by_url.json");
    const EMPTY: &str = include_str!("../../tests/fixtures/m2_hn_search_empty.json");
    const ITEM: &str = include_str!("../../tests/fixtures/m2_hn_item.json");

    fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z".parse().unwrap()
    }

    #[test]
    fn comments_url_yields_the_story_id() {
        assert_eq!(
            story_id_from_comments_url("https://news.ycombinator.com/item?id=41234567").as_deref(),
            Some("41234567")
        );
        assert_eq!(
            story_id_from_comments_url("http://news.ycombinator.com/item?id=1&foo=bar").as_deref(),
            Some("1")
        );
        assert_eq!(story_id_from_comments_url("https://lobste.rs/s/abc"), None);
        assert_eq!(
            story_id_from_comments_url("https://news.ycombinator.com/newest"),
            None
        );
        assert_eq!(
            story_id_from_comments_url("https://news.ycombinator.com/item?id=abc"),
            None
        );
        assert_eq!(story_id_from_comments_url(""), None);
    }

    #[test]
    fn search_response_picks_the_matching_submission() {
        let got = parse_search_response(SEARCH, "https://blog.dev/post", 7, ts())
            .unwrap()
            .expect("a hit");
        assert_eq!(got.article_id, 7);
        assert_eq!(got.source, SocialSource::Hn);
        assert_eq!(got.item_id.as_deref(), Some("41234567"));
        assert_eq!(got.score, 342);
        assert_eq!(got.num_comments, 210);
        assert_eq!(
            got.item_url.as_deref(),
            Some("https://news.ycombinator.com/item?id=41234567")
        );
        assert_eq!(got.fetched_at, ts());
    }

    #[test]
    fn search_response_falls_back_to_the_top_hit() {
        // No hit canonicalizes to this URL, so the highest-scoring one wins.
        let got = parse_search_response(SEARCH, "https://elsewhere.dev/x", 7, ts())
            .unwrap()
            .expect("a hit");
        assert_eq!(got.item_id.as_deref(), Some("41234567"));
        assert_eq!(got.score, 342);
    }

    #[test]
    fn empty_search_response_is_not_an_error() {
        assert!(
            parse_search_response(EMPTY, "https://blog.dev/never-submitted", 1, ts())
                .unwrap()
                .is_none()
        );
        assert!(parse_story_response(EMPTY, 1, ts()).unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_reported_not_panicked() {
        let err = parse_search_response("{not json", "https://x.dev", 1, ts()).unwrap_err();
        assert!(matches!(
            err,
            SocialError::Unexpected { platform: "hn", .. }
        ));
    }

    #[test]
    fn item_response_becomes_a_comment_tree() {
        let thread = parse_item_response(ITEM).unwrap();
        assert_eq!(thread.source, SocialSource::Hn);
        assert_eq!(
            thread.item_url,
            "https://news.ycombinator.com/item?id=41234567"
        );
        // 5 comment nodes in the fixture (one of them deleted).
        assert_eq!(thread.total_comments, 5);
        // The deleted, childless comment is dropped from the render tree.
        assert_eq!(thread.comments.len(), 2);

        let first = &thread.comments[0];
        assert_eq!(first.author, "dbnerd");
        assert_eq!(first.points, Some(88));
        assert_eq!(first.depth, 0);
        assert!(first.text_html.contains("page splits"));
        // <i> is not in the allowlist, its text survives.
        assert!(!first.text_html.contains("<i>"));
        assert!(first.text_html.contains("Bookmarked."));

        let reply = &first.children[0];
        assert_eq!(reply.author, "tylerh");
        assert_eq!(reply.depth, 1);
        assert_eq!(reply.children[0].depth, 2);
        assert_eq!(thread.comments[1].author, "skeptic");
    }
}
