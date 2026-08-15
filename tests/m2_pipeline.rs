//! Milestone 2 integration test: Miniflux entries → dedupe → extraction (§3.2, §3.3).
//!
//! No network: the extractor is built with [`Extractor::offline`], so the fetch
//! leg of the §3.3 priority chain is skipped and the Miniflux/excerpt legs run
//! exactly as they do in production (notes §6).

use jiff::Timestamp;

use daily_epub::extract::{self, Extractor};
use daily_epub::types::{Article, Entry, ExtractMethod, SourceKind};
use daily_epub::{dedupe, miniflux};

fn ts(s: &str) -> Timestamp {
    s.parse().expect("timestamp")
}

fn body(words: usize) -> String {
    format!("<p>{}</p>", "sqlite pages and btrees ".repeat(words / 4))
}

/// One page of ingest output: the same story from three feeds plus some noise.
fn ingested() -> Vec<Entry> {
    let base = Entry {
        id: 0,
        feed_id: 0,
        feed_title: None,
        category: Some("Tech".into()),
        title: String::new(),
        url: String::new(),
        canonical_url: None,
        author: None,
        published_at: Some(ts("2026-08-15T04:00:00Z")),
        comments_url: None,
        raw_content: String::new(),
        fetched_at: ts("2026-08-15T05:30:00Z"),
    };

    vec![
        // 1. HN frontpage: title + link only, plus the discussion link.
        Entry {
            id: 101,
            feed_id: 1,
            feed_title: Some("Hacker News Front Page".into()),
            title: "A Deep Dive Into B-Trees".into(),
            url: "https://blog.dev/b-trees?utm_source=hnrss".into(),
            comments_url: Some("https://news.ycombinator.com/item?id=41234567".into()),
            raw_content: "<p>Comments</p>".into(),
            ..base.clone()
        },
        // 2. Scour: same story, same URL modulo the fragment, richer summary.
        Entry {
            id: 102,
            feed_id: 2,
            feed_title: Some("Scour: Databases".into()),
            title: "A Deep Dive Into B-Trees".into(),
            url: "https://blog.dev/b-trees#intro".into(),
            raw_content: format!(
                r#"<p>Intro.</p><figure><img src="/img/split.png" alt="a page split"></figure>{}"#,
                body(700)
            ),
            published_at: Some(ts("2026-08-15T03:00:00Z")),
            ..base.clone()
        },
        // 3. The blog's own feed: different URL, same story (title pass merges it).
        Entry {
            id: 103,
            feed_id: 3,
            feed_title: Some("blog.dev".into()),
            title: "A Deep Dive into B-Trees!".into(),
            url: "https://blog.dev/2026/08/b-trees".into(),
            author: Some("Dana Author".into()),
            raw_content: "<p>Short summary of the post.</p>".into(),
            published_at: Some(ts("2026-08-15T02:00:00Z")),
            ..base.clone()
        },
        // 4. A different story, feed excerpt only.
        Entry {
            id: 104,
            feed_id: 4,
            feed_title: Some("Lobsters".into()),
            title: "Notes on Writing a Toy Allocator".into(),
            url: "https://other.dev/allocator".into(),
            comments_url: Some("https://lobste.rs/s/abcdef/notes".into()),
            raw_content: "<p>A teaser paragraph and nothing else.</p>".into(),
            ..base.clone()
        },
        // 5–7. Non-articles: video host, media enclosure, empty title.
        Entry {
            id: 105,
            feed_id: 5,
            feed_title: Some("Video Feed".into()),
            title: "A conference talk".into(),
            url: "https://www.youtube.com/watch?v=abc".into(),
            raw_content: "<p>watch it</p>".into(),
            ..base.clone()
        },
        Entry {
            id: 106,
            feed_id: 6,
            feed_title: Some("A Podcast".into()),
            title: "Episode 42".into(),
            url: "https://cdn.dev/episodes/42.mp3".into(),
            raw_content: "<audio src=\"https://cdn.dev/episodes/42.mp3\"></audio>".into(),
            ..base.clone()
        },
        Entry {
            id: 107,
            feed_id: 7,
            feed_title: Some("Broken Feed".into()),
            title: "   ".into(),
            url: "https://broken.dev/x".into(),
            raw_content: body(500),
            ..base.clone()
        },
    ]
}

#[tokio::test]
async fn dedupe_then_extract_produces_ready_articles() {
    let (mut articles, stats) = dedupe::cluster(ingested());

    // --- §3.2 ---
    assert_eq!(stats.entries_in, 7);
    assert_eq!(stats.dropped_non_article, 3);
    assert_eq!(stats.clusters, 2);
    assert_eq!(stats.merged, 1);
    assert_eq!(articles.len(), 2);

    let deep_dive = &articles[0];
    assert_eq!(deep_dive.canonical_url, "https://blog.dev/b-trees");
    assert_eq!(deep_dive.id, 0, "not persisted yet");
    assert_eq!(deep_dive.best_entry_id, 102, "richest content wins");
    assert_eq!(deep_dive.sources.len(), 3);
    assert!(deep_dive.came_via(SourceKind::HnFrontpage));
    assert!(deep_dive.came_via(SourceKind::Scour));
    assert!(deep_dive.came_via(SourceKind::Feed));
    assert_eq!(deep_dive.author.as_deref(), Some("Dana Author"));
    assert_eq!(deep_dive.first_seen, ts("2026-08-15T02:00:00Z"));
    assert_eq!(
        deep_dive.comments_url.as_deref(),
        Some("https://news.ycombinator.com/item?id=41234567")
    );
    assert_eq!(deep_dive.chapter_id(), "art-102");

    // --- §3.3 ---
    let extractor = Extractor::offline(vec![]);
    assert!(!extractor.can_fetch(), "tests never hit the network");
    let extract_stats = extractor.extract_all(&mut articles).await;
    assert_eq!(extract_stats.from_miniflux, 1);
    assert_eq!(extract_stats.from_readability, 0);
    assert_eq!(extract_stats.excerpt_only, 1);

    let deep_dive = &articles[0];
    assert_eq!(deep_dive.extract_method, ExtractMethod::Miniflux);
    assert!(deep_dive.word_count >= extract::FULL_TEXT_MIN_WORDS);
    assert!(!deep_dive.excerpt_only);
    // Sanitized body, relative image resolved against the entry URL.
    assert!(
        deep_dive.content_html.contains("<figure>"),
        "allowlisted tag"
    );
    assert!(
        deep_dive
            .content_html
            .contains(r#"src="https://blog.dev/img/split.png""#)
    );
    assert_eq!(deep_dive.image_count, 1);
    assert_eq!(deep_dive.image_urls, ["https://blog.dev/img/split.png"]);

    let allocator = &articles[1];
    assert_eq!(allocator.canonical_url, "https://other.dev/allocator");
    assert_eq!(allocator.extract_method, ExtractMethod::Excerpt);
    assert!(allocator.excerpt_only, "penalized by the pre-filter (§3.5)");
    assert!(allocator.content_html.contains(extract::EXCERPT_NOTE));
    assert!(allocator.came_via(SourceKind::Lobsters));
    assert_eq!(allocator.image_count, 0);
}

#[tokio::test]
async fn rerunning_the_stage_is_idempotent() {
    let (mut once, _) = dedupe::cluster(ingested());
    let extractor = Extractor::offline(vec![]);
    extractor.extract_all(&mut once).await;

    // Feeding the already-extracted articles back in changes nothing: the
    // sanitized body is still full text, so the Miniflux leg wins again.
    let before: Vec<Article> = once.clone();
    extractor.extract_all(&mut once).await;
    assert_eq!(once, before);
}

#[tokio::test]
async fn paywalled_stubs_are_marked_excerpt_only() {
    let entry = Entry {
        id: 200,
        feed_id: 20,
        feed_title: Some("NYT".into()),
        category: None,
        title: "A Paywalled Story".into(),
        url: "https://www.nytimes.com/2026/08/15/story.html".into(),
        canonical_url: None,
        author: None,
        published_at: Some(ts("2026-08-15T04:00:00Z")),
        comments_url: None,
        // Long enough to pass the full-text bar, short enough to smell like a stub.
        raw_content: body(300),
        fetched_at: ts("2026-08-15T05:30:00Z"),
    };
    let (mut articles, _) = dedupe::cluster(vec![entry]);
    Extractor::offline(vec![]).extract_all(&mut articles).await;

    assert_eq!(articles[0].extract_method, ExtractMethod::Miniflux);
    assert!(articles[0].word_count >= extract::FULL_TEXT_MIN_WORDS);
    assert!(articles[0].word_count < extract::PAYWALL_MAX_WORDS);
    assert!(articles[0].excerpt_only, "known paywall host + short body");
}

/// A Scour interest feed whose *title* says nothing about Scour is still
/// recognized when the run's `feed_id → url` map is threaded through (§3.2).
#[tokio::test]
async fn feed_urls_make_scour_detection_exact() {
    let entry = Entry {
        id: 300,
        feed_id: 30,
        // Scour names its feeds after the interest, not after itself.
        feed_title: Some("Rust".into()),
        category: Some("Interests".into()),
        title: "Async Cancellation, Revisited".into(),
        url: "https://blog.dev/cancellation".into(),
        canonical_url: None,
        author: None,
        published_at: Some(ts("2026-08-15T04:00:00Z")),
        comments_url: None,
        raw_content: body(600),
        fetched_at: ts("2026-08-15T05:30:00Z"),
    };

    // Without the map, the title/category give nothing away.
    let (plain, _) = dedupe::cluster(vec![entry.clone()]);
    assert!(!plain[0].came_via(SourceKind::Scour));

    let feeds = std::collections::HashMap::from([(
        30,
        daily_epub::miniflux::FeedMeta {
            id: 30,
            title: "Rust".into(),
            site_url: "https://scour.ing".into(),
            feed_url: "https://scour.ing/feed?interest=rust&token=secret".into(),
            category: Some("Interests".into()),
        },
    )]);
    let (exact, _) = dedupe::cluster_with_feeds(vec![entry], &miniflux::feed_urls(&feeds));
    assert!(
        exact[0].came_via(SourceKind::Scour),
        "the feed URL is the only reliable Scour tell"
    );
}
