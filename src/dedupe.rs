//! Normalization and duplicate clustering (spec §3.2).
//!
//! Canonicalizes URLs, clusters entries that tell the same story (HN frontpage feed
//! + Scour feed + the blog's own feed), and drops obvious non-articles.

use std::collections::HashMap;

use jiff::Timestamp;
use percent_encoding::percent_decode_str;
use url::Url;

use crate::types::{Article, Entry, ExtractMethod, SourceKind, SourceRef};

/// Query parameters stripped during canonicalization (§3.2).
pub const TRACKING_PARAMS: &[&str] = &["ref", "fbclid", "gclid", "s", "si", "mc_cid", "mc_eid"];

/// URL hosts that are never articles (§3.2).
pub const NON_ARTICLE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "youtu.be",
    "vimeo.com",
    "open.spotify.com",
    "podcasts.apple.com",
];

/// Path suffixes that mark an audio/video enclosure rather than an article (§3.2).
const MEDIA_EXTENSIONS: &[&str] = &[
    ".mp3", ".m4a", ".m4v", ".mp4", ".ogg", ".oga", ".opus", ".wav", ".flac", ".aac", ".mov",
    ".webm", ".mkv",
];

/// How many redirector hops [`canonical_url`] will follow before giving up (§3.2).
const MAX_REDIRECT_DEPTH: u8 = 3;

/// Minimum length of a [`normalized_title`] before it may merge two clusters.
/// Short titles ("News", "Weekly") collide far too easily (§3.2 secondary pass).
const MIN_TITLE_KEY_LEN: usize = 12;

/// Canonicalize a URL: lowercase host, drop the fragment, strip tracking params
/// (`utm_*` and [`TRACKING_PARAMS`]), trim the trailing slash, and resolve known
/// redirectors such as Google News links to their target (§3.2).
///
/// Returns `None` when the input is not a parseable absolute http(s) URL.
pub fn canonical_url(raw: &str) -> Option<String> {
    canonicalize(raw, 0)
}

fn canonicalize(raw: &str, depth: u8) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut url = Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;

    // Known redirectors (Google News et al) carry the real article in a param.
    if depth < MAX_REDIRECT_DEPTH
        && let Some(target) = redirect_target(&url)
        && let Some(resolved) = canonicalize(&target, depth + 1)
    {
        return Some(resolved);
    }

    url.set_fragment(None);

    if let Some(host) = url.host_str() {
        let lower = host.to_ascii_lowercase();
        if lower != host {
            url.set_host(Some(&lower)).ok()?;
        }
    }

    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        for (k, v) in &kept {
            pairs.append_pair(k, v);
        }
        drop(pairs);
    }

    let path = url.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        url.set_path(path.trim_end_matches('/'));
    }

    let mut out = url.to_string();
    if url.query().is_none() && out.ends_with('/') {
        out.pop();
    }
    Some(out)
}

fn is_tracking_param(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_") || TRACKING_PARAMS.contains(&key.as_str())
}

/// The real destination behind a known redirector, if any (§3.2).
fn redirect_target(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if matches!(host.as_str(), "scour.ing" | "www.scour.ing")
        && let Some(encoded) = url.path().strip_prefix("/r/rss/")
    {
        let target = percent_decode_str(encoded).decode_utf8().ok()?.into_owned();
        let target_url = Url::parse(&target).ok()?;
        if matches!(target_url.scheme(), "http" | "https") && target_url.host_str().is_some() {
            return Some(target);
        }
        return None;
    }
    let is_google_news = host == "news.google.com" || host.ends_with(".news.google.com");
    let is_google_redirect = host == "news.url.google.com"
        || ((host == "www.google.com" || host == "google.com") && url.path() == "/url");
    if !(is_google_news || is_google_redirect) {
        return None;
    }
    url.query_pairs()
        .find(|(k, _)| k == "url" || k == "q")
        .map(|(_, v)| v.into_owned())
        .filter(|v| v.starts_with("http"))
}

/// Title normalized for the fuzzy second dedupe pass: lowercased, alphanumeric only (§3.2).
pub fn normalized_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// True for entries that are not articles at all: media-enclosure-only items,
/// [`NON_ARTICLE_HOSTS`], empty titles (§3.2).
pub fn is_non_article(entry: &Entry) -> bool {
    if entry.title.trim().is_empty() {
        return true;
    }
    let Some(url) = Url::parse(entry.url.trim()).ok().filter(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    }) else {
        return true;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if NON_ARTICLE_HOSTS
        .iter()
        .any(|blocked| host == *blocked || host.ends_with(&format!(".{blocked}")))
    {
        return true;
    }
    let path = url.path().to_ascii_lowercase();
    if MEDIA_EXTENSIONS.iter().any(|ext| path.ends_with(ext)) {
        return true;
    }
    is_enclosure_only(&entry.raw_content)
}

/// Content that is nothing but an embedded player has no text worth reading (§3.2).
fn is_enclosure_only(raw_content: &str) -> bool {
    let lower = raw_content.to_ascii_lowercase();
    let embeds = lower.contains("<audio")
        || lower.contains("<video")
        || lower.contains("<embed")
        || lower.contains("<iframe");
    embeds && crate::html::word_count(raw_content) < 25
}

/// Classify which kind of feed an entry arrived through, for the sources list (§3.2, §3.5).
pub fn classify_source(entry: &Entry) -> SourceKind {
    classify_source_with_feed(entry, None)
}

/// [`classify_source`] with the feed's own URL/site URL when the caller has it.
///
/// The `entries` table does not store the feed URL, so the entry-only form falls
/// back to the feed title plus the entry/comments URLs (§3.2).
pub fn classify_source_with_feed(entry: &Entry, feed_url_or_site: Option<&str>) -> SourceKind {
    let mut haystack = String::new();
    if let Some(feed) = feed_url_or_site {
        haystack.push_str(&feed.to_ascii_lowercase());
        haystack.push(' ');
    }
    if let Some(title) = &entry.feed_title {
        haystack.push_str(&title.to_ascii_lowercase());
        haystack.push(' ');
    }
    if let Some(category) = &entry.category {
        haystack.push_str(&category.to_ascii_lowercase());
        haystack.push(' ');
    }
    let comments = entry
        .comments_url
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let url = entry.url.to_ascii_lowercase();

    if haystack.contains("scour.ing") || haystack.contains("scour") {
        return SourceKind::Scour;
    }
    if haystack.contains("hnrss")
        || haystack.contains("news.ycombinator")
        || haystack.contains("hacker news")
        || comments.contains("news.ycombinator.com")
    {
        return SourceKind::HnFrontpage;
    }
    if haystack.contains("lobste.rs")
        || haystack.contains("lobsters")
        || comments.contains("lobste.rs")
        || url.contains("lobste.rs/s/")
    {
        return SourceKind::Lobsters;
    }
    if haystack.contains("reddit.com")
        || haystack.contains("reddit")
        || comments.contains("reddit.com")
        || url.contains("reddit.com/r/")
    {
        return SourceKind::Reddit;
    }
    SourceKind::Feed
}

/// `feed_id → feed URL (or site URL)`, as built by [`crate::miniflux::feed_urls`].
///
/// Passing it into [`cluster_with_feeds`] is what makes "came via Scour" exact:
/// a Scour interest feed is only recognizable from its `feed_url`, and the
/// `entries` table does not store one (§3.2).
pub type FeedUrls = HashMap<crate::types::FeedId, String>;

/// Build a [`SourceRef`] describing how `entry` reached us.
pub fn source_ref(entry: &Entry) -> SourceRef {
    source_ref_with_feeds(entry, &FeedUrls::new())
}

/// [`source_ref`] with the run's `feed_id → feed url` map for exact classification.
pub fn source_ref_with_feeds(entry: &Entry, feed_urls: &FeedUrls) -> SourceRef {
    SourceRef {
        entry_id: entry.id,
        feed_id: entry.feed_id,
        feed_title: entry
            .feed_title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("feed {}", entry.feed_id)),
        category: entry.category.clone(),
        kind: classify_source_with_feed(entry, feed_urls.get(&entry.feed_id).map(String::as_str)),
    }
}

/// Outcome of the dedupe stage.
#[derive(Debug, Clone, Default)]
pub struct DedupeStats {
    pub entries_in: usize,
    pub dropped_non_article: usize,
    pub clusters: usize,
    /// Clusters that merged more than one entry.
    pub merged: usize,
}

/// Cluster `entries` into [`Article`]s: primary key is the canonical URL, secondary
/// pass matches [`normalized_title`] within the window. Each cluster keeps the
/// richest content, the union of source refs and the earliest `first_seen` (§3.2).
///
/// The returned articles have `id == 0` (not yet persisted) and carry the richest
/// *raw* Miniflux content in `content_html`; [`crate::extract`] replaces it with the
/// sanitized body and the real `word_count`.
pub fn cluster(entries: Vec<Entry>) -> (Vec<Article>, DedupeStats) {
    cluster_with_feeds(entries, &FeedUrls::new())
}

/// [`cluster`] with the run's `feed_id → feed url` map, so `SourceKind::Scour`
/// (and the other feed-shaped kinds) are detected from the feed URL rather than
/// guessed from the feed title (§3.2).
pub fn cluster_with_feeds(
    entries: Vec<Entry>,
    feed_urls: &FeedUrls,
) -> (Vec<Article>, DedupeStats) {
    let span = tracing::info_span!("dedupe", entries = entries.len());
    let _guard = span.enter();

    let mut stats = DedupeStats {
        entries_in: entries.len(),
        ..DedupeStats::default()
    };

    let mut clusters: Vec<Vec<(Entry, String)>> = Vec::new();
    let mut by_url: HashMap<String, usize> = HashMap::new();
    let mut by_title: HashMap<String, usize> = HashMap::new();

    for entry in entries {
        if is_non_article(&entry) {
            stats.dropped_non_article += 1;
            continue;
        }
        let Some(canon) = canonical_url(&entry.url) else {
            stats.dropped_non_article += 1;
            continue;
        };
        let title_key = normalized_title(&entry.title);
        let title_key = (title_key.len() >= MIN_TITLE_KEY_LEN).then_some(title_key);

        let index = match by_url.get(&canon) {
            Some(&i) => i,
            None => match title_key.as_ref().and_then(|k| by_title.get(k)) {
                Some(&i) => i,
                None => {
                    clusters.push(Vec::new());
                    clusters.len() - 1
                }
            },
        };
        by_url.entry(canon.clone()).or_insert(index);
        if let Some(key) = title_key {
            by_title.entry(key).or_insert(index);
        }
        clusters[index].push((entry, canon));
    }

    let mut articles: Vec<Article> = Vec::with_capacity(clusters.len());
    for members in clusters {
        if members.is_empty() {
            continue;
        }
        if members.len() > 1 {
            stats.merged += 1;
        }
        articles.push(build_article(members, feed_urls));
    }
    stats.clusters = articles.len();

    tracing::info!(
        clusters = stats.clusters,
        merged = stats.merged,
        dropped = stats.dropped_non_article,
        "clustered entries into articles"
    );
    (articles, stats)
}

/// Merge one cluster's entries into a single [`Article`], keeping the richest body.
fn build_article(members: Vec<(Entry, String)>, feed_urls: &FeedUrls) -> Article {
    // Richest content wins; ties break on the lowest entry id so runs are stable.
    let best = members
        .iter()
        .enumerate()
        .max_by_key(|(_, (entry, _))| (crate::html::word_count(&entry.raw_content), -(entry.id)))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let (best_entry, canonical) = &members[best];

    let first_seen = members
        .iter()
        .map(|(e, _)| e.published_at.unwrap_or(e.fetched_at))
        .min()
        .unwrap_or_else(Timestamp::now);
    let published_at = members.iter().filter_map(|(e, _)| e.published_at).min();

    let mut sources: Vec<SourceRef> = members
        .iter()
        .map(|(e, _)| source_ref_with_feeds(e, feed_urls))
        .collect();
    sources.sort_by_key(|s| s.entry_id);
    sources.dedup_by_key(|s| s.entry_id);

    // Prefer a comments URL that actually points at a discussion we can look up.
    let comments_url = members
        .iter()
        .filter_map(|(e, _)| e.comments_url.clone())
        .filter(|c| !c.trim().is_empty())
        .max_by_key(|c| {
            let lower = c.to_ascii_lowercase();
            if lower.contains("news.ycombinator.com") {
                2
            } else if lower.contains("lobste.rs") {
                1
            } else {
                0
            }
        });

    // An aggregator entry's author is usually the submitter, so it only counts
    // when no direct feed carried the story; extraction may still replace it
    // with the page's own byline.
    let is_direct = |e: &Entry| {
        classify_source_with_feed(e, feed_urls.get(&e.feed_id).map(String::as_str))
            == SourceKind::Feed
    };
    let has_direct = members.iter().any(|(e, _)| is_direct(e));
    let author = members
        .iter()
        .filter(|(e, _)| !has_direct || is_direct(e))
        .filter_map(|(e, _)| e.author.clone())
        .find(|a| !a.trim().is_empty());

    let word_count = crate::html::word_count(&best_entry.raw_content);

    Article {
        id: 0,
        canonical_url: canonical.clone(),
        title: best_entry.title.trim().to_string(),
        best_entry_id: best_entry.id,
        content_html: best_entry.raw_content.clone(),
        word_count,
        excerpt_only: false,
        image_count: 0,
        sources,
        first_seen,
        url: best_entry.url.clone(),
        author,
        publication: None,
        feed_id: best_entry.feed_id,
        feed_title: best_entry
            .feed_title
            .clone()
            .unwrap_or_else(|| format!("feed {}", best_entry.feed_id)),
        category: best_entry.category.clone(),
        published_at,
        comments_url,
        image_urls: Vec::new(),
        social: Vec::new(),
        extract_method: ExtractMethod::Miniflux,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn entry(id: i64, url: &str, title: &str) -> Entry {
        Entry {
            id,
            feed_id: id * 10,
            feed_title: Some(format!("Feed {id}")),
            category: Some("Tech".into()),
            title: title.into(),
            url: url.into(),
            canonical_url: None,
            author: None,
            published_at: Some(ts("2026-08-15T04:00:00Z")),
            comments_url: None,
            raw_content: "<p>hello world</p>".into(),
            fetched_at: ts("2026-08-15T05:30:00Z"),
        }
    }

    #[test]
    fn canonicalization_table() {
        let cases: &[(&str, Option<&str>)] = &[
            // host case + fragment
            (
                "https://Example.COM/Posts/One#section",
                Some("https://example.com/Posts/One"),
            ),
            // trailing slash
            ("https://example.com/a/b/", Some("https://example.com/a/b")),
            // bare root loses its slash
            ("https://example.com/", Some("https://example.com")),
            ("http://example.com", Some("http://example.com")),
            // utm_* and friends
            (
                "https://example.com/p?utm_source=rss&utm_medium=feed&utm_campaign=x",
                Some("https://example.com/p"),
            ),
            (
                "https://example.com/p?ref=hn&fbclid=abc&gclid=def&s=1&si=2",
                Some("https://example.com/p"),
            ),
            // meaningful params survive
            (
                "https://example.com/p?id=7&utm_source=rss",
                Some("https://example.com/p?id=7"),
            ),
            // mixed: everything at once
            (
                "HTTPS://WWW.Example.com/Path/?utm_source=a&page=2#frag",
                Some("https://www.example.com/Path?page=2"),
            ),
            // google news redirector resolves to the target
            (
                "https://news.google.com/rss/articles/CBMi?oc=5&url=https%3A%2F%2Fexample.com%2Freal%2F",
                Some("https://example.com/real"),
            ),
            (
                "https://www.google.com/url?q=https://example.com/real&sa=D",
                Some("https://example.com/real"),
            ),
            // Scour RSS redirectors encode the real URL in the path
            (
                "https://scour.ing/r/rss/https%3A%2F%2Frmzlb.github.io%2Fnotifyd%2Farticles%2Fpostgres-queue-what-skip-locked-does-not-give-you.html",
                Some(
                    "https://rmzlb.github.io/notifyd/articles/postgres-queue-what-skip-locked-does-not-give-you.html",
                ),
            ),
            (
                "https://www.scour.ing/r/rss/https%3A%2F%2Fexample.com%2Fpost%3Fid%3D7%26utm_source%3Dscour%26utm_medium%3Drss",
                Some("https://example.com/post?id=7"),
            ),
            (
                "https://scour.ing/@tyler/interests/Kernel%20Development",
                Some("https://scour.ing/@tyler/interests/Kernel%20Development"),
            ),
            // non-http schemes and junk
            ("mailto:tyler@hallada.net", None),
            ("ftp://example.com/file", None),
            ("not a url", None),
            ("", None),
            ("   ", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                canonical_url(input).as_deref(),
                *expected,
                "canonicalizing {input:?}"
            );
        }
    }

    #[test]
    fn canonicalization_is_idempotent() {
        let once = canonical_url("https://Example.com/A/?utm_source=x#y").unwrap();
        assert_eq!(canonical_url(&once).as_deref(), Some(once.as_str()));
    }

    #[test]
    fn title_normalization() {
        assert_eq!(
            normalized_title("Rust 1.90: What's *New*?"),
            "rust190whatsnew"
        );
        assert_eq!(
            normalized_title("  The   Quick — Brown Fox  "),
            "thequickbrownfox"
        );
        // Same story, different feed punctuation, same key.
        assert_eq!(
            normalized_title("Show HN: My Tiny Database"),
            normalized_title("Show HN – My Tiny Database!")
        );
        assert_eq!(normalized_title("!!!"), "");
    }

    #[test]
    fn non_articles_are_rejected() {
        let mut youtube = entry(1, "https://www.youtube.com/watch?v=abc", "A video");
        assert!(is_non_article(&youtube));
        youtube.url = "https://m.youtube.com/watch?v=abc".into();
        assert!(is_non_article(&youtube));

        assert!(is_non_article(&entry(
            2,
            "https://open.spotify.com/episode/x",
            "An episode"
        )));
        assert!(is_non_article(&entry(3, "https://example.com/p", "   ")));
        assert!(is_non_article(&entry(4, "javascript:void(0)", "Bad url")));
        assert!(is_non_article(&entry(
            5,
            "https://cdn.example.com/ep/12.mp3",
            "Episode 12"
        )));

        let mut enclosure = entry(6, "https://example.com/pod/12", "Episode 12");
        enclosure.raw_content = "<audio src=\"https://x/1.mp3\"></audio>".into();
        assert!(is_non_article(&enclosure));

        // An article that merely embeds a video is still an article.
        let mut with_video = entry(7, "https://example.com/post", "A real post");
        with_video.raw_content =
            format!("<iframe src=\"x\"></iframe><p>{}</p>", "word ".repeat(60));
        assert!(!is_non_article(&with_video));

        assert!(!is_non_article(&entry(8, "https://example.com/p", "Fine")));
    }

    #[test]
    fn source_kinds_come_from_feed_metadata() {
        let mut e = entry(1, "https://example.com/p", "T");
        e.feed_title = Some("Scour: Rust".into());
        assert_eq!(classify_source(&e), SourceKind::Scour);

        e.feed_title = Some("Hacker News: Front Page".into());
        assert_eq!(classify_source(&e), SourceKind::HnFrontpage);

        e.feed_title = Some("Some Blog".into());
        e.comments_url = Some("https://news.ycombinator.com/item?id=1".into());
        assert_eq!(classify_source(&e), SourceKind::HnFrontpage);

        e.comments_url = Some("https://lobste.rs/s/abcdef/thing".into());
        assert_eq!(classify_source(&e), SourceKind::Lobsters);

        e.comments_url = None;
        e.feed_title = Some("r/rust".into());
        e.url = "https://www.reddit.com/r/rust/comments/x/y/".into();
        assert_eq!(classify_source(&e), SourceKind::Reddit);

        e.feed_title = Some("Tyler's Blog".into());
        e.category = Some("Blogroll".into());
        e.url = "https://hallada.net/post".into();
        assert_eq!(classify_source(&e), SourceKind::Feed);

        // The feed URL wins when the caller has it.
        assert_eq!(
            classify_source_with_feed(&e, Some("https://scour.ing/feed/rust")),
            SourceKind::Scour
        );
    }

    #[test]
    fn clustering_merges_by_url_then_title() {
        let long_body = format!("<p>{}</p>", "word ".repeat(400));

        // Same story from three feeds: two share a URL (modulo tracking params),
        // the third differs only in punctuation of the title.
        let mut hn = entry(
            1,
            "https://blog.dev/post?utm_source=hn",
            "A Deep Dive Into B-Trees",
        );
        hn.feed_title = Some("Hacker News".into());
        hn.comments_url = Some("https://news.ycombinator.com/item?id=42".into());

        let mut scour = entry(2, "https://blog.dev/post/", "A Deep Dive Into B-Trees");
        scour.feed_title = Some("Scour: Databases".into());
        scour.raw_content = long_body.clone();

        let mut own = entry(3, "https://blog.dev/post-alt", "A Deep Dive into B-Trees!");
        own.feed_title = Some("Blog.dev".into());
        own.published_at = Some(ts("2026-08-15T02:00:00Z"));

        let other = entry(4, "https://other.dev/x", "Something Else Entirely Here");

        let (articles, stats) = cluster(vec![hn, scour, own, other]);
        assert_eq!(stats.entries_in, 4);
        assert_eq!(stats.clusters, 2);
        assert_eq!(stats.merged, 1);
        assert_eq!(stats.dropped_non_article, 0);

        let merged = &articles[0];
        assert_eq!(merged.canonical_url, "https://blog.dev/post");
        assert_eq!(merged.sources.len(), 3);
        // Richest content won.
        assert_eq!(merged.best_entry_id, 2);
        assert!(merged.word_count > 300);
        // Union of source kinds, used as a curation signal.
        assert!(merged.came_via(SourceKind::Scour));
        assert!(merged.came_via(SourceKind::HnFrontpage));
        assert!(merged.came_via(SourceKind::Feed));
        // Earliest publication time and the HN comments link survive the merge.
        assert_eq!(merged.first_seen, ts("2026-08-15T02:00:00Z"));
        assert_eq!(
            merged.comments_url.as_deref(),
            Some("https://news.ycombinator.com/item?id=42")
        );
        assert_eq!(merged.id, 0);

        assert_eq!(articles[1].canonical_url, "https://other.dev/x");
        assert_eq!(articles[1].sources.len(), 1);
    }

    #[test]
    fn direct_feed_author_beats_aggregator_submitter() {
        let mut aggregator = entry(1, "https://blog.dev/post", "A Distinct Article Title");
        aggregator.author = Some("HN Submitter".into());
        aggregator.raw_content = format!("<p>{}</p>", "word ".repeat(50));

        let mut direct = entry(2, "https://blog.dev/post", "A Distinct Article Title");
        direct.author = Some("Real Writer".into());

        let feed_urls = FeedUrls::from([
            (aggregator.feed_id, "https://hnrss.org/frontpage".into()),
            (direct.feed_id, "https://blog.dev/feed.xml".into()),
        ]);
        let (articles, _) = cluster_with_feeds(vec![aggregator, direct], &feed_urls);

        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].best_entry_id, 1);
        assert_eq!(articles[0].author.as_deref(), Some("Real Writer"));

        // With a direct feed present, a submitter name is not used as a fallback.
        let mut aggregator = entry(3, "https://blog.dev/other", "Another Distinct Title");
        aggregator.author = Some("HN Submitter".into());
        let mut direct = entry(4, "https://blog.dev/other", "Another Distinct Title");
        direct.author = None;
        let feed_urls = FeedUrls::from([
            (aggregator.feed_id, "https://hnrss.org/frontpage".into()),
            (direct.feed_id, "https://blog.dev/feed.xml".into()),
        ]);
        let (articles, _) = cluster_with_feeds(vec![aggregator, direct], &feed_urls);
        assert_eq!(articles[0].author, None);
    }

    #[test]
    fn clustering_drops_non_articles_and_keeps_short_titles_apart() {
        let mut a = entry(1, "https://a.dev/1", "News");
        a.raw_content = "<p>one</p>".into();
        let mut b = entry(2, "https://b.dev/2", "News");
        b.raw_content = "<p>two</p>".into();
        let video = entry(3, "https://youtu.be/xyz", "A video");

        let (articles, stats) = cluster(vec![a, b, video]);
        assert_eq!(stats.dropped_non_article, 1);
        // "news" is below MIN_TITLE_KEY_LEN, so the two stay separate.
        assert_eq!(articles.len(), 2);
        assert_eq!(stats.merged, 0);
    }
}
