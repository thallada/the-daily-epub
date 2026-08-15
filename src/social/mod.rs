//! Social-proof enrichment (spec §3.4).
//!
//! For every deduped article, look up HackerNews (Algolia), Lobsters and Reddit
//! in parallel behind a semaphore, caching results in the `social` table. Every
//! lookup is best-effort: failures never fail the run (notes §3).

pub mod hn;
pub mod lobsters;
pub mod reddit;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use jiff::Timestamp;
use tokio::sync::{Mutex, Semaphore};

use crate::db::Db;
use crate::types::{Article, ArticleId, SocialRef, SocialSource, SourceKind};

/// Concurrent social lookups (§3.4).
pub const CONCURRENCY: usize = 8;
/// Reddit pacing: roughly one request per second (§3.4).
pub const REDDIT_MIN_INTERVAL_MS: u64 = 1000;
/// Cached rows younger than this are reused instead of re-fetched (§3.4).
pub const CACHE_TTL_HOURS: i64 = 24;

#[derive(Debug, thiserror::Error)]
pub enum SocialError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected response from {platform}: {detail}")]
    Unexpected {
        platform: &'static str,
        detail: String,
    },
    #[error("rate limited by {0}")]
    RateLimited(&'static str),
}

/// Serializes a platform's requests to at most one per `interval` (§3.4 Reddit).
#[derive(Debug, Clone)]
struct Pacer {
    last: Arc<Mutex<Option<tokio::time::Instant>>>,
    interval: Duration,
}

impl Pacer {
    fn new(interval: Duration) -> Self {
        Self {
            last: Arc::new(Mutex::new(None)),
            interval,
        }
    }

    /// Block until the caller may issue the next request.
    async fn tick(&self) {
        let mut last = self.last.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
        }
        *last = Some(tokio::time::Instant::now());
    }
}

/// The identifiers one article needs for its three lookups (§3.4).
#[derive(Debug, Clone)]
struct Lookup {
    article_id: ArticleId,
    canonical_url: String,
    /// HN story id from `comments_url`, when the feed handed us one.
    hn_story_id: Option<String>,
    /// Lobsters story id — only available for lobste.rs-originated entries (§7).
    lobsters_story_id: Option<String>,
}

impl Lookup {
    fn for_article(article: &Article) -> Self {
        let comments_url = article.comments_url.as_deref().unwrap_or("");
        let hn_story_id = hn::story_id_from_comments_url(comments_url);

        // Lobsters linkage requires a lobste.rs origin: either the feed itself or
        // a comments URL pointing at a story (§3.4, §7).
        let via_lobsters = article.came_via(SourceKind::Lobsters);
        let lobsters_story_id = lobsters::story_id_from_url(comments_url).or_else(|| {
            via_lobsters
                .then(|| lobsters::story_id_from_url(&article.url))
                .flatten()
        });

        Self {
            article_id: article.id,
            canonical_url: article.canonical_url.clone(),
            hn_story_id,
            lobsters_story_id,
        }
    }
}

/// Orchestrates the per-platform clients and the `social` cache (§3.4).
#[derive(Debug, Clone)]
pub struct SocialEnricher {
    http: reqwest::Client,
    db: Db,
    reddit_pacer: Pacer,
    /// Set once Reddit 429s: the rest of the run skips Reddit entirely (§3.4).
    reddit_blocked: Arc<AtomicBool>,
}

impl SocialEnricher {
    pub fn new(http: reqwest::Client, db: Db) -> Self {
        Self {
            http,
            db,
            reddit_pacer: Pacer::new(Duration::from_millis(REDDIT_MIN_INTERVAL_MS)),
            reddit_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Enrich every article in place, writing hits to the `social` table (§3.4).
    ///
    /// Returns the number of articles for which at least one platform had a hit.
    pub async fn enrich_all(&self, articles: &mut [Article]) -> usize {
        self.run(articles, false).await
    }

    /// [`Self::enrich_all`] ignoring the cache — used by `backfill-social` (§2).
    pub async fn refresh_all(&self, articles: &mut [Article]) -> usize {
        self.run(articles, true).await
    }

    async fn run(&self, articles: &mut [Article], force: bool) -> usize {
        let span = tracing::info_span!("social", articles = articles.len(), force);
        let _guard = span.enter();

        let lookups: Vec<Lookup> = articles.iter().map(Lookup::for_article).collect();
        let semaphore = Arc::new(Semaphore::new(CONCURRENCY));
        let results: Vec<(usize, Vec<SocialRef>)> =
            futures::stream::iter(lookups.iter().enumerate())
                .map(|(i, lookup)| {
                    let semaphore = Arc::clone(&semaphore);
                    async move {
                        // A closed semaphore is impossible here; treat it as "no limit".
                        let _permit = semaphore.acquire().await.ok();
                        (i, self.lookup(lookup, force).await)
                    }
                })
                .buffer_unordered(CONCURRENCY)
                .collect()
                .await;

        let mut hits = 0;
        for (i, refs) in results {
            if !refs.is_empty() {
                hits += 1;
            }
            articles[i].social = refs;
        }
        tracing::info!(hits, "social enrichment complete");
        hits
    }

    /// Look up every platform for one article, honoring the cache TTL (§3.4).
    pub async fn enrich_one(&self, article: &Article) -> Vec<SocialRef> {
        self.lookup(&Lookup::for_article(article), false).await
    }

    async fn lookup(&self, target: &Lookup, force: bool) -> Vec<SocialRef> {
        let mut refs: Vec<SocialRef> = if force || target.article_id == 0 {
            Vec::new()
        } else {
            cached_refs(&self.db, target.article_id).await
        };
        let cached = refs.len();

        if !refs.iter().any(|r| r.source == SocialSource::Hn)
            && let Some(found) = self.lookup_hn(target).await
        {
            refs.push(found);
        }
        if !refs.iter().any(|r| r.source == SocialSource::Lobsters)
            && let Some(found) = self.lookup_lobsters(target).await
        {
            refs.push(found);
        }
        if !refs.iter().any(|r| r.source == SocialSource::Reddit)
            && let Some(found) = self.lookup_reddit(target).await
        {
            refs.push(found);
        }

        // Persist only what we just fetched; cached rows are already stored.
        if target.article_id != 0 {
            for r in refs.iter().skip(cached) {
                if let Err(e) = self.db.upsert_social(r).await {
                    tracing::warn!(
                        article = target.article_id,
                        "storing social ref failed: {e}"
                    );
                }
            }
        }
        refs.sort_by_key(|r| r.source);
        refs
    }

    async fn lookup_hn(&self, target: &Lookup) -> Option<SocialRef> {
        let result = match &target.hn_story_id {
            Some(id) => hn::fetch_story(&self.http, id, target.article_id).await,
            None => hn::search_by_url(&self.http, &target.canonical_url, target.article_id).await,
        };
        best_effort("hn", &target.canonical_url, result)
    }

    async fn lookup_lobsters(&self, target: &Lookup) -> Option<SocialRef> {
        let id = target.lobsters_story_id.as_deref()?;
        let result = lobsters::fetch_story(&self.http, id, target.article_id).await;
        best_effort("lobsters", &target.canonical_url, result)
    }

    async fn lookup_reddit(&self, target: &Lookup) -> Option<SocialRef> {
        if self.reddit_blocked.load(Ordering::Relaxed) {
            return None;
        }
        self.reddit_pacer.tick().await;
        let result =
            reddit::lookup_by_url(&self.http, &target.canonical_url, target.article_id).await;
        if matches!(result, Err(SocialError::RateLimited(_))) {
            // Back off for the rest of the run: social data is best-effort (§3.4).
            self.reddit_blocked.store(true, Ordering::Relaxed);
            tracing::warn!("reddit rate-limited us; skipping reddit for the rest of this run");
            return None;
        }
        best_effort("reddit", &target.canonical_url, result)
    }

    /// Re-poll recent articles' social scores (`daily-epub backfill-social`, §2).
    pub async fn backfill(&self, days: u32) -> anyhow::Result<usize> {
        let span = tracing::info_span!("backfill_social", days);
        let _guard = span.enter();

        let cutoff = Timestamp::now() - jiff::Span::new().hours(24 * i64::from(days.max(1)));
        let rows = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM articles WHERE first_seen >= ? ORDER BY first_seen DESC",
        )
        .bind(crate::db::fmt_ts(cutoff))
        .fetch_all(self.db.pool())
        .await?;

        let mut articles: Vec<Article> = Vec::with_capacity(rows.len());
        for id in rows {
            match self.db.get_article(id).await {
                Ok(Some(article)) => articles.push(article),
                Ok(None) => {}
                Err(e) => tracing::warn!(article = id, "loading article failed: {e}"),
            }
        }
        tracing::info!(articles = articles.len(), "re-polling social scores");
        Ok(self.refresh_all(&mut articles).await)
    }
}

/// Log-and-drop wrapper: no social lookup may ever fail the run (notes §3).
fn best_effort(
    platform: &'static str,
    url: &str,
    result: Result<Option<SocialRef>, SocialError>,
) -> Option<SocialRef> {
    match result {
        Ok(found) => found,
        Err(e) => {
            tracing::debug!(platform, url, "social lookup failed: {e}");
            None
        }
    }
}

/// Load cached refs for an article, ignoring rows older than [`CACHE_TTL_HOURS`].
pub async fn cached_refs(db: &Db, article_id: ArticleId) -> Vec<SocialRef> {
    match db.social_for_article(article_id).await {
        Ok(refs) => {
            let now = Timestamp::now();
            refs.into_iter().filter(|r| is_fresh(r, now)).collect()
        }
        Err(e) => {
            tracing::warn!(article = article_id, "reading social cache failed: {e}");
            Vec::new()
        }
    }
}

/// True while a cached row is younger than [`CACHE_TTL_HOURS`] (§3.4).
pub fn is_fresh(social_ref: &SocialRef, now: Timestamp) -> bool {
    // A negative age (clock skew, a row written moments ago) is fresh too.
    now.as_second() - social_ref.fetched_at.as_second() < CACHE_TTL_HOURS * 3600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtractMethod, SourceRef};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn article(url: &str) -> Article {
        Article {
            id: 1,
            canonical_url: url.into(),
            title: "T".into(),
            best_entry_id: 1,
            content_html: "<p>x</p>".into(),
            word_count: 1,
            excerpt_only: false,
            image_count: 0,
            sources: vec![SourceRef {
                entry_id: 1,
                feed_id: 1,
                feed_title: "Feed".into(),
                category: None,
                kind: SourceKind::Feed,
            }],
            first_seen: ts("2026-08-15T05:00:00Z"),
            url: url.into(),
            author: None,
            feed_id: 1,
            feed_title: "Feed".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        }
    }

    fn social(source: SocialSource, fetched_at: Timestamp) -> SocialRef {
        SocialRef {
            article_id: 1,
            source,
            item_id: Some("1".into()),
            score: 10,
            num_comments: 5,
            item_url: None,
            fetched_at,
        }
    }

    #[test]
    fn cache_freshness_window() {
        let now = ts("2026-08-15T12:00:00Z");
        assert!(is_fresh(&social(SocialSource::Hn, now), now));
        assert!(is_fresh(
            &social(SocialSource::Hn, ts("2026-08-14T13:00:00Z")),
            now
        ));
        assert!(!is_fresh(
            &social(SocialSource::Hn, ts("2026-08-14T11:00:00Z")),
            now
        ));
        // Clock skew (a row from the future) counts as fresh, never as ancient.
        assert!(is_fresh(
            &social(SocialSource::Hn, ts("2026-08-15T13:00:00Z")),
            now
        ));
    }

    #[test]
    fn lookup_targets_come_from_comments_urls_and_sources() {
        let mut a = article("https://blog.dev/post");
        a.comments_url = Some("https://news.ycombinator.com/item?id=41234567".into());
        let l = Lookup::for_article(&a);
        assert_eq!(l.hn_story_id.as_deref(), Some("41234567"));
        assert_eq!(l.lobsters_story_id, None);
        assert_eq!(l.canonical_url, "https://blog.dev/post");

        a.comments_url = Some("https://lobste.rs/s/abcdef/a_deep_dive".into());
        let l = Lookup::for_article(&a);
        assert_eq!(l.hn_story_id, None);
        assert_eq!(l.lobsters_story_id.as_deref(), Some("abcdef"));

        // No comments URL and no lobsters origin: no lobsters lookup at all (§7).
        a.comments_url = None;
        assert_eq!(Lookup::for_article(&a).lobsters_story_id, None);

        // Lobsters-origin entry whose URL is the story itself.
        a.url = "https://lobste.rs/s/zzzzzz/title".into();
        a.sources[0].kind = SourceKind::Lobsters;
        assert_eq!(
            Lookup::for_article(&a).lobsters_story_id.as_deref(),
            Some("zzzzzz")
        );
    }

    #[tokio::test]
    async fn pacer_serializes_requests() {
        let pacer = Pacer::new(Duration::from_millis(30));
        let started = std::time::Instant::now();
        pacer.tick().await;
        pacer.tick().await;
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn cache_reads_skip_stale_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("t.db"))
            .await
            .unwrap();
        db.upsert_entry(&crate::types::Entry {
            id: 1,
            feed_id: 1,
            feed_title: Some("Feed".into()),
            category: None,
            title: "T".into(),
            url: "https://blog.dev/post".into(),
            canonical_url: Some("https://blog.dev/post".into()),
            author: None,
            published_at: None,
            comments_url: None,
            raw_content: "<p>x</p>".into(),
            fetched_at: Timestamp::now(),
        })
        .await
        .unwrap();
        let id = db
            .upsert_article(&article("https://blog.dev/post"))
            .await
            .unwrap();

        db.upsert_social(&SocialRef {
            article_id: id,
            fetched_at: Timestamp::now(),
            ..social(SocialSource::Hn, Timestamp::now())
        })
        .await
        .unwrap();
        db.upsert_social(&SocialRef {
            article_id: id,
            fetched_at: Timestamp::now() - jiff::Span::new().hours(48),
            ..social(SocialSource::Reddit, Timestamp::now())
        })
        .await
        .unwrap();

        let fresh = cached_refs(&db, id).await;
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].source, SocialSource::Hn);
    }
}
