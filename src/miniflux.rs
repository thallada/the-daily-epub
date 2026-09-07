//! Miniflux API client (spec §3.1).
//!
//! Almost reads only: entries are fetched with `published_after` inside the
//! lookback window **regardless of read/unread status**, and read state is never
//! mutated so normal reader usage is undisturbed. The single writing call is
//! [`MinifluxClient::create_feed`], which the dashboard's feed-discovery page
//! uses to subscribe to a candidate the operator picked (feed discovery plan
//! §4 step 2); the pipeline never calls it.

use std::collections::HashMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::MinifluxConfig;
use crate::http::{RetryPolicy, is_retryable};
use crate::types::{Entry, FeedId};

/// Miniflux caps `limit` at 250 (§3.1).
pub const MAX_PAGE_LIMIT: u32 = 250;
/// Safety valve so a misconfigured window cannot page forever.
const MAX_PAGES: u32 = 200;

#[derive(Debug, thiserror::Error)]
pub enum MinifluxError {
    #[error("miniflux api key is not configured (set DAILY_EPUB_MINIFLUX__API_KEY)")]
    MissingApiKey,
    #[error("miniflux request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("miniflux returned {status} for {path}: {message}")]
    Status {
        status: u16,
        path: String,
        /// `error_message` when the body was Miniflux's JSON error envelope,
        /// else the truncated body — what a dashboard flash should show.
        message: String,
    },
    #[error("could not parse miniflux response for {path}: {source}")]
    Decode {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, MinifluxError>;

impl MinifluxError {
    /// Build a [`MinifluxError::Status`], pulling `error_message` out of
    /// Miniflux's JSON error envelope when the body has one (§4 step 2).
    fn status(status: u16, path: &str, body: &str) -> Self {
        #[derive(Deserialize)]
        struct ErrorBody {
            error_message: String,
        }
        let body: String = body.chars().take(300).collect();
        let message = serde_json::from_str::<ErrorBody>(&body)
            .map(|e| e.error_message)
            .unwrap_or_else(|_| body.clone());
        MinifluxError::Status {
            status,
            path: path.to_string(),
            message,
        }
    }

    /// True for the `400 This feed already exists.` a duplicate `POST /v1/feeds`
    /// returns — the dashboard flips the row to `added` on it instead of
    /// treating it as a failure.
    pub fn is_duplicate_feed(&self) -> bool {
        matches!(
            self,
            MinifluxError::Status { status: 400, message, .. }
                if message.trim().eq_ignore_ascii_case("This feed already exists.")
        )
    }
}

/// Network failures and 5xx/429 responses are worth another attempt (§3.1).
fn retryable(error: &MinifluxError) -> bool {
    match error {
        MinifluxError::Http(e) => is_retryable(e),
        MinifluxError::Status { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Wire types (only the fields §3.1 lists as used)
// ---------------------------------------------------------------------------

/// A category as embedded in a feed object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinifluxCategory {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
}

/// `GET /v1/feeds` element — used to build the `feed_id → metadata` map (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinifluxFeed {
    pub id: FeedId,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub site_url: String,
    #[serde(default)]
    pub feed_url: String,
    #[serde(default)]
    pub category: Option<MinifluxCategory>,
}

/// `GET /v1/entries` element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinifluxEntry {
    pub id: i64,
    pub feed_id: FeedId,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub comments_url: String,
    #[serde(default)]
    pub author: String,
    /// RFC3339 with offset, e.g. `2026-08-15T04:00:00-04:00`.
    #[serde(default)]
    pub published_at: String,
    /// Miniflux's stored content: full text when "fetch original content" is on.
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub reading_time: i64,
    /// Present when Miniflux inlines the feed object on the entry.
    #[serde(default)]
    pub feed: Option<MinifluxFeed>,
}

/// Envelope returned by `GET /v1/entries`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntriesResponse {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub entries: Vec<MinifluxEntry>,
}

/// `POST /v1/discover` element — a *lead*, not a verified feed: Miniflux
/// returns well-known paths that merely answered 200 (plan §2), so every one is
/// validated before it becomes a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovered {
    #[serde(default)]
    pub url: String,
    /// Miniflux substitutes the URL when the `<link>` tag has no title, which is
    /// how a well-known-path guess is told apart from a real link-tag hit.
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "type")]
    pub kind: String,
}

/// `POST /v1/feeds` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedFeed {
    pub feed_id: i64,
}

/// Feed metadata joined onto every entry we persist (§3.1).
#[derive(Debug, Clone, PartialEq)]
pub struct FeedMeta {
    pub id: FeedId,
    pub title: String,
    pub site_url: String,
    /// The subscribed feed URL — the only reliable "came via Scour" tell (§3.2).
    pub feed_url: String,
    pub category: Option<String>,
}

impl From<&MinifluxFeed> for FeedMeta {
    fn from(f: &MinifluxFeed) -> Self {
        Self {
            id: f.id,
            title: f.title.clone(),
            site_url: f.site_url.clone(),
            feed_url: f.feed_url.clone(),
            category: f.category.as_ref().map(|c| c.title.clone()),
        }
    }
}

/// `feed_id → "{feed_url} {site_url}"`, the haystack
/// [`crate::dedupe::classify_source_with_feed`] matches against (§3.2).
pub fn feed_urls(feeds: &HashMap<FeedId, FeedMeta>) -> crate::dedupe::FeedUrls {
    feeds
        .iter()
        .map(|(id, meta)| {
            (
                *id,
                format!("{} {}", meta.feed_url, meta.site_url)
                    .trim()
                    .to_string(),
            )
        })
        .collect()
}

impl MinifluxEntry {
    /// Parse `published_at`, tolerating the empty/zero values Miniflux can emit.
    pub fn published_timestamp(&self) -> Option<Timestamp> {
        if self.published_at.is_empty() {
            return None;
        }
        self.published_at.parse::<Timestamp>().ok()
    }

    fn opt(s: &str) -> Option<String> {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    /// Convert to the persisted [`Entry`] shape (§3.13).
    ///
    /// `canonical_url` is left `None`: the dedupe stage (§3.2) fills it in.
    pub fn into_entry(self, feeds: &HashMap<FeedId, FeedMeta>, fetched_at: Timestamp) -> Entry {
        let meta = feeds.get(&self.feed_id);
        let inline = self.feed.as_ref();
        let feed_title = meta
            .map(|m| m.title.clone())
            .or_else(|| inline.map(|f| f.title.clone()))
            .filter(|t| !t.is_empty());
        let category = meta
            .and_then(|m| m.category.clone())
            .or_else(|| {
                inline
                    .and_then(|f| f.category.as_ref())
                    .map(|c| c.title.clone())
            })
            .filter(|c| !c.is_empty());
        Entry {
            id: self.id,
            feed_id: self.feed_id,
            feed_title,
            category,
            published_at: self.published_timestamp(),
            title: self.title.trim().to_string(),
            url: self.url.trim().to_string(),
            canonical_url: None,
            author: Self::opt(&self.author),
            comments_url: Self::opt(&self.comments_url),
            raw_content: self.content,
            fetched_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Read-only Miniflux client (§3.1).
#[derive(Debug, Clone)]
pub struct MinifluxClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    page_limit: u32,
    retry: RetryPolicy,
}

impl MinifluxClient {
    /// Build a client from `[miniflux]` config; fails if the API key is absent.
    pub fn new(cfg: &MinifluxConfig, http: reqwest::Client) -> Result<Self> {
        let api_key = cfg
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or(MinifluxError::MissingApiKey)?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key,
            page_limit: cfg.page_limit.clamp(1, MAX_PAGE_LIMIT),
            retry: RetryPolicy::default(),
        })
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1{}", self.base_url, path)
    }

    /// GET `path` with the auth header, retrying network/5xx failures (§3.1).
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path);
        let body = self
            .retry
            .run(&format!("GET {path}"), retryable, || async {
                let resp = self
                    .http
                    .get(&url)
                    .header("X-Auth-Token", &self.api_key)
                    .header("Accept", "application/json")
                    .query(query)
                    .send()
                    .await?;
                let status = resp.status();
                let text = resp.text().await?;
                if !status.is_success() {
                    return Err(MinifluxError::status(status.as_u16(), path, &text));
                }
                Ok(text)
            })
            .await?;
        serde_json::from_str(&body).map_err(|source| MinifluxError::Decode {
            path: path.to_string(),
            source,
        })
    }

    /// One `POST path` with a JSON body, no retry.
    async fn post_once<B: Serialize>(&self, path: &str, body: &B) -> Result<String> {
        let resp = self
            .http
            .post(self.url(path))
            .header("X-Auth-Token", &self.api_key)
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(MinifluxError::status(status.as_u16(), path, &text));
        }
        Ok(text)
    }

    /// POST `path` with the auth header, mirroring [`Self::get_json`].
    ///
    /// `retry` is false for calls that are not safe to repeat: a retried
    /// `POST /v1/feeds` whose response was merely lost would subscribe twice.
    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        retry: bool,
    ) -> Result<T> {
        let text = if retry {
            self.retry
                .run(&format!("POST {path}"), retryable, || {
                    self.post_once(path, body)
                })
                .await?
        } else {
            self.post_once(path, body).await?
        };
        serde_json::from_str(&text).map_err(|source| MinifluxError::Decode {
            path: path.to_string(),
            source,
        })
    }

    /// `POST /v1/discover` — the feed(s) Miniflux can find behind a page URL
    /// (feed discovery plan §4 step 2). Retried: it changes nothing.
    pub async fn discover(&self, url: &str) -> Result<Vec<Discovered>> {
        self.post_json("/discover", &serde_json::json!({ "url": url }), true)
            .await
    }

    /// `GET /v1/categories` — the Add form's category picker.
    pub async fn categories(&self) -> Result<Vec<MinifluxCategory>> {
        self.get_json("/categories", &[]).await
    }

    /// `POST /v1/feeds` — subscribe to `feed_url` in `category_id`, returning
    /// the new feed id. Never retried (a repeat can double-subscribe).
    pub async fn create_feed(&self, feed_url: &str, category_id: i64) -> Result<i64> {
        let created: CreatedFeed = self
            .post_json(
                "/feeds",
                &serde_json::json!({ "feed_url": feed_url, "category_id": category_id }),
                false,
            )
            .await?;
        Ok(created.feed_id)
    }

    /// `GET /v1/feeds` — once per run (§3.1).
    pub async fn feeds(&self) -> Result<Vec<MinifluxFeed>> {
        self.get_json("/feeds", &[]).await
    }

    /// `feed_id → {title, site_url, category.title}` (§3.1).
    pub async fn feed_map(&self) -> Result<HashMap<FeedId, FeedMeta>> {
        Ok(self
            .feeds()
            .await?
            .iter()
            .map(|f| (f.id, FeedMeta::from(f)))
            .collect())
    }

    /// One page of `GET /v1/entries`, ordered by `published_at` desc (§3.1).
    pub async fn entries_page(
        &self,
        published_after: Timestamp,
        offset: u32,
    ) -> Result<EntriesResponse> {
        self.get_json(
            "/entries",
            &[
                ("order", "published_at".to_string()),
                ("direction", "desc".to_string()),
                ("published_after", published_after.as_second().to_string()),
                ("limit", self.page_limit.to_string()),
                ("offset", offset.to_string()),
            ],
        )
        .await
    }

    /// Page through every entry published after `published_after`, read or not (§3.1).
    pub async fn entries_since(&self, published_after: Timestamp) -> Result<Vec<MinifluxEntry>> {
        let mut all: Vec<MinifluxEntry> = Vec::new();
        let mut offset = 0u32;
        for page in 0..MAX_PAGES {
            let resp = self.entries_page(published_after, offset).await?;
            let got = resp.entries.len();
            tracing::debug!(
                page,
                offset,
                got,
                total = resp.total,
                "miniflux entries page"
            );
            all.extend(resp.entries);
            if got < self.page_limit as usize || all.len() as i64 >= resp.total {
                break;
            }
            offset += self.page_limit;
        }
        Ok(all)
    }

    /// Full ingest: fetch the feed map, page the window, and map to [`Entry`] rows.
    ///
    /// Entries published after `until` (the run's "now") are dropped so a
    /// re-run for a past date does not pull in newer stories.
    pub async fn ingest_window(
        &self,
        since: Timestamp,
        until: Timestamp,
        fetched_at: Timestamp,
    ) -> Result<(Vec<Entry>, HashMap<FeedId, FeedMeta>)> {
        let feeds = self.feed_map().await?;
        tracing::info!(feeds = feeds.len(), "loaded miniflux feed metadata");
        let raw = self.entries_since(since).await?;
        tracing::info!(entries = raw.len(), "fetched miniflux entries");
        let entries = raw
            .into_iter()
            .filter(|e| match e.published_timestamp() {
                Some(ts) => ts <= until,
                // Keep entries with unparseable dates; dedupe will judge them.
                None => true,
            })
            .map(|e| e.into_entry(&feeds, fetched_at))
            .collect();
        Ok((entries, feeds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRIES_JSON: &str = r#"{
      "total": 2,
      "entries": [
        {
          "id": 30011,
          "user_id": 1,
          "feed_id": 42,
          "status": "unread",
          "hash": "abc",
          "title": "  Writing a Kernel in Rust  ",
          "url": "https://example.com/kernel-rust",
          "comments_url": "https://news.ycombinator.com/item?id=44551122",
          "published_at": "2026-08-15T04:12:00-04:00",
          "created_at": "2026-08-15T08:13:00Z",
          "author": "Jane Dev",
          "content": "<p>A long post.</p>",
          "starred": false,
          "reading_time": 14,
          "enclosures": null,
          "feed": {
            "id": 42,
            "title": "Inline Feed Title",
            "site_url": "https://example.com",
            "feed_url": "https://example.com/feed.xml",
            "category": {"id": 3, "title": "Inline Category"}
          }
        },
        {
          "id": 30012,
          "feed_id": 99,
          "status": "read",
          "title": "No feed object here",
          "url": "https://other.example/post",
          "comments_url": "",
          "published_at": "",
          "author": "",
          "content": "",
          "starred": true,
          "reading_time": 0
        }
      ]
    }"#;

    const FEEDS_JSON: &str = r#"[
      {"id": 42, "title": "Lobsters", "site_url": "https://lobste.rs",
       "feed_url": "https://lobste.rs/rss", "category": {"id": 1, "title": "Tech"}},
      {"id": 99, "title": "Scour: Rust", "site_url": "https://scour.ing",
       "feed_url": "https://scour.ing/feed", "category": null}
    ]"#;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn deserializes_entries_response() {
        let resp: EntriesResponse = serde_json::from_str(ENTRIES_JSON).unwrap();
        assert_eq!(resp.total, 2);
        assert_eq!(resp.entries.len(), 2);
        let first = &resp.entries[0];
        assert_eq!(first.id, 30011);
        assert_eq!(first.feed_id, 42);
        assert_eq!(
            first.comments_url,
            "https://news.ycombinator.com/item?id=44551122"
        );
        assert_eq!(
            first.published_timestamp(),
            Some(ts("2026-08-15T08:12:00Z"))
        );
        assert_eq!(first.feed.as_ref().unwrap().title, "Inline Feed Title");
        assert!(resp.entries[1].published_timestamp().is_none());
    }

    #[test]
    fn deserializes_feeds_and_builds_map() {
        let feeds: Vec<MinifluxFeed> = serde_json::from_str(FEEDS_JSON).unwrap();
        let map: HashMap<FeedId, FeedMeta> =
            feeds.iter().map(|f| (f.id, FeedMeta::from(f))).collect();
        assert_eq!(map[&42].title, "Lobsters");
        assert_eq!(map[&42].category.as_deref(), Some("Tech"));
        assert_eq!(map[&99].category, None);
    }

    #[test]
    fn maps_entries_onto_feed_metadata() {
        let feeds: Vec<MinifluxFeed> = serde_json::from_str(FEEDS_JSON).unwrap();
        let map: HashMap<FeedId, FeedMeta> =
            feeds.iter().map(|f| (f.id, FeedMeta::from(f))).collect();
        let resp: EntriesResponse = serde_json::from_str(ENTRIES_JSON).unwrap();
        let fetched = ts("2026-08-15T09:30:00Z");
        let entries: Vec<Entry> = resp
            .entries
            .into_iter()
            .map(|e| e.into_entry(&map, fetched))
            .collect();

        // /v1/feeds metadata wins over the inline feed object.
        assert_eq!(entries[0].feed_title.as_deref(), Some("Lobsters"));
        assert_eq!(entries[0].category.as_deref(), Some("Tech"));
        assert_eq!(entries[0].title, "Writing a Kernel in Rust");
        assert_eq!(entries[0].author.as_deref(), Some("Jane Dev"));
        assert!(entries[0].comments_url.is_some());
        assert_eq!(entries[0].canonical_url, None);
        assert_eq!(entries[0].fetched_at, fetched);

        // Empty strings become None, not "".
        assert_eq!(entries[1].author, None);
        assert_eq!(entries[1].comments_url, None);
        assert_eq!(entries[1].feed_title.as_deref(), Some("Scour: Rust"));
        assert_eq!(entries[1].category, None);
    }

    /// Observed live for an article URL whose page carries `<link rel=alternate>`
    /// tags: the same feed offered twice under one title (plan §2).
    const DISCOVER_LINK_TAGS_JSON: &str = r#"[
      {"title":"blog.philz.dev","url":"https://blog.philz.dev/feed/feed.xml","type":"atom"},
      {"title":"blog.philz.dev","url":"https://blog.philz.dev/feed/feed.json","type":"json"}
    ]"#;

    /// Observed live for a site that answers 200 for any path: nine unverified
    /// well-known-path guesses, each with `title == url` (plan §2).
    const DISCOVER_GUESSES_JSON: &str = r#"[
      {"title":"https://zombo.com/atom.xml","url":"https://zombo.com/atom.xml","type":"atom"},
      {"title":"https://zombo.com/feed.atom","url":"https://zombo.com/feed.atom","type":"atom"},
      {"title":"https://zombo.com/feed.xml","url":"https://zombo.com/feed.xml","type":"atom"},
      {"title":"https://zombo.com/feed/","url":"https://zombo.com/feed/","type":"atom"},
      {"title":"https://zombo.com/index.rss","url":"https://zombo.com/index.rss","type":"rss"},
      {"title":"https://zombo.com/index.xml","url":"https://zombo.com/index.xml","type":"rss"},
      {"title":"https://zombo.com/rss.xml","url":"https://zombo.com/rss.xml","type":"rss"},
      {"title":"https://zombo.com/rss/","url":"https://zombo.com/rss/","type":"rss"},
      {"title":"https://zombo.com/rss/feed.xml","url":"https://zombo.com/rss/feed.xml","type":"rss"}
    ]"#;

    const CATEGORIES_JSON: &str = r#"[
      {"id": 1, "user_id": 1, "title": "Tech", "hide_globally": false},
      {"id": 4, "user_id": 1, "title": "Long reads", "hide_globally": true}
    ]"#;

    #[test]
    fn deserializes_discover_results() {
        let hits: Vec<Discovered> = serde_json::from_str(DISCOVER_LINK_TAGS_JSON).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "blog.philz.dev");
        assert_eq!(hits[0].url, "https://blog.philz.dev/feed/feed.xml");
        assert_eq!(hits[0].kind, "atom");
        assert_eq!(hits[1].kind, "json");

        let guesses: Vec<Discovered> = serde_json::from_str(DISCOVER_GUESSES_JSON).unwrap();
        assert_eq!(guesses.len(), 9);
        assert!(guesses.iter().all(|g| g.title == g.url));
    }

    #[test]
    fn deserializes_categories_and_created_feed() {
        let categories: Vec<MinifluxCategory> = serde_json::from_str(CATEGORIES_JSON).unwrap();
        assert_eq!(categories.len(), 2);
        assert_eq!(categories[1].id, 4);
        assert_eq!(categories[1].title, "Long reads");

        let created: CreatedFeed = serde_json::from_str(r#"{"feed_id": 123}"#).unwrap();
        assert_eq!(created.feed_id, 123);
    }

    #[test]
    fn status_errors_carry_the_error_message() {
        let duplicate = MinifluxError::status(
            400,
            "/feeds",
            r#"{"error_message":"This feed already exists."}"#,
        );
        assert!(duplicate.is_duplicate_feed());
        assert_eq!(
            duplicate.to_string(),
            "miniflux returned 400 for /feeds: This feed already exists."
        );

        let fetcher = MinifluxError::status(
            502,
            "/discover",
            r#"{"error_message":"fetcher: bad gateway (502 status code)"}"#,
        );
        assert!(!fetcher.is_duplicate_feed());
        assert!(retryable(&fetcher));
        match fetcher {
            MinifluxError::Status { message, .. } => {
                assert_eq!(message, "fetcher: bad gateway (502 status code)");
            }
            other => panic!("unexpected error {other:?}"),
        }

        // A non-JSON body falls back to the truncated body itself.
        let html = MinifluxError::status(404, "/discover", "<html>nope</html>");
        assert!(!retryable(&html));
        assert_eq!(
            html.to_string(),
            "miniflux returned 404 for /discover: <html>nope</html>"
        );
    }

    #[test]
    fn requires_an_api_key() {
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let mut cfg = MinifluxConfig::default();
        assert!(matches!(
            MinifluxClient::new(&cfg, http.clone()),
            Err(MinifluxError::MissingApiKey)
        ));
        cfg.api_key = Some("   ".into());
        assert!(MinifluxClient::new(&cfg, http.clone()).is_err());
        cfg.api_key = Some("token".into());
        cfg.base_url = "http://127.0.0.1:8082/".into();
        cfg.page_limit = 5000;
        let c = MinifluxClient::new(&cfg, http).unwrap();
        assert_eq!(c.base_url, "http://127.0.0.1:8082");
        assert_eq!(c.page_limit, MAX_PAGE_LIMIT);
        assert_eq!(c.url("/entries"), "http://127.0.0.1:8082/v1/entries");
    }
}
