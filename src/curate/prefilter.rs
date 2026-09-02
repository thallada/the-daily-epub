//! Heuristic pre-filter: 300–500 articles → ~120 candidates (spec §3.5).
//!
//! Pure Rust and free: this is what keeps LLM cost flat as feed volume grows.
//!
//! The 0–100 score is a sum of bounded components so that no single signal can
//! dominate, and every component is monotonic in its input:
//!
//! | component | range | source |
//! |---|---|---|
//! | long-form word count | 0 … +35 | §3.5 "0 pts <300 words, max at ~2500+" |
//! | social proof | 0 … +25 | §3.4 composite, log-scaled again |
//! | came via Scour | +8 | §3.5 (already matched a stated interest) |
//! | came via HN frontpage | +8 | §3.5 |
//! | carried by several feeds | 0 … +8 | §3.2 (multi-source *is* social proof) |
//! | excerpt only | −20 | §3.5 (penalized, never banned — §7) |
//! | roundup/release-notes title | −15 | §3.5 |
//! | blocked domain | excluded | §3.5 |

use std::collections::HashSet;

use crate::config::{Config, CurationConfig};
use crate::types::{Article, ArticleId, FeedId, ScoredArticle, SourceKind};

/// Title patterns that mark low-effort posts: link roundups, release notes,
/// sponsor posts (§3.5).
pub const PENALTY_TITLE_PATTERNS: &[&str] = &[
    "link roundup",
    "links for",
    "weekly digest",
    "release notes",
    "changelog",
    "sponsored",
    "this week in",
    "linkdump",
    "link dump",
    "weekly roundup",
    "roundup:",
    "in case you missed it",
    "what we're reading",
    "sponsor post",
    "now available",
    "is now generally available",
    "release candidate",
    "patch notes",
    "job board",
    "who's hiring",
    "newsletter #",
    "digest #",
];

/// Word count at which the long-form bonus saturates (§3.5).
pub const LONGFORM_SATURATION_WORDS: i64 = 2500;
/// Below this word count the long-form bonus is zero (§3.5).
pub const LONGFORM_FLOOR_WORDS: i64 = 300;
/// Articles the LLM scored below this within the last week are not re-scored (§3.5).
pub const STALE_LOW_SCORE: f64 = 3.0;
/// Lookback for the "don't re-score churn" rule (§3.5).
pub const STALE_LOOKBACK_DAYS: i64 = 7;

/// Maximum contribution of each scoring component (§3.5).
pub const MAX_LONGFORM_POINTS: f64 = 35.0;
pub const MAX_SOCIAL_POINTS: f64 = 25.0;
pub const SCOUR_BONUS: f64 = 8.0;
pub const HN_FRONTPAGE_BONUS: f64 = 8.0;
pub const MAX_MULTI_SOURCE_POINTS: f64 = 8.0;
pub const EXCERPT_ONLY_PENALTY: f64 = 20.0;
pub const ROUNDUP_TITLE_PENALTY: f64 = 15.0;

/// `composite_social_score` value that earns the full social bonus. Empirically
/// ~6.0 is a 1,000-point HN story with 500 comments (§3.4 formula).
const SOCIAL_SATURATION: f64 = 6.0;

/// Everything the pre-filter needs beyond the articles themselves (§3.5, §3.9).
#[derive(Debug, Clone, Default)]
pub struct PrefilterContext {
    /// Article ids already published in a previous issue (§3.5).
    pub already_published: Vec<ArticleId>,
    /// Article ids the LLM scored < [`STALE_LOW_SCORE`] recently (§3.5).
    pub recently_rejected: Vec<ArticleId>,
}

impl PrefilterContext {
    /// Load the history/priors context from SQLite (§3.5 dedup-vs-history, §3.9).
    ///
    /// `today` anchors the [`STALE_LOOKBACK_DAYS`] window.
    pub async fn load(
        db: &crate::db::Db,
        today: jiff::civil::Date,
    ) -> Result<Self, crate::db::DbError> {
        let since = today
            .checked_sub(jiff::Span::new().days(STALE_LOOKBACK_DAYS))
            .unwrap_or(today);
        let already_published = db.previously_published_ids_before(today).await?;
        let recently_rejected = db.recently_low_scored_ids(STALE_LOW_SCORE, since).await?;
        tracing::debug!(
            published = already_published.len(),
            rejected = recently_rejected.len(),
            "loaded prefilter context"
        );
        Ok(Self {
            already_published,
            recently_rejected,
        })
    }
}

/// True when the article's feed is in `curation.always_include_feeds` (§3.5).
///
/// Entries are matched either as a Miniflux feed id (any feed in the cluster) or
/// as a case-insensitive substring of the article/site URL.
///
/// Auto-includes are still LLM-scored (for section + summary) but can't be dropped.
pub fn is_auto_include(article: &Article, cfg: &CurationConfig) -> bool {
    if cfg.always_include_feeds.is_empty() {
        return false;
    }
    let url = article.url.to_lowercase();
    let canonical = article.canonical_url.to_lowercase();
    cfg.always_include_feeds.iter().any(|raw| {
        let needle = raw.trim();
        if needle.is_empty() {
            return false;
        }
        if let Ok(id) = needle.parse::<FeedId>()
            && (article.feed_id == id || article.sources.iter().any(|s| s.feed_id == id))
        {
            return true;
        }
        let needle = needle.to_lowercase();
        // Bare host or full site URL: compare against both URLs we hold.
        let needle = needle
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        !needle.is_empty() && (url.contains(needle) || canonical.contains(needle))
    })
}

/// True when the article's host matches `curation.blocked_domains` (§3.5).
pub fn is_blocked(article: &Article, cfg: &CurationConfig) -> bool {
    if cfg.blocked_domains.is_empty() {
        return false;
    }
    let host = host_of(&article.canonical_url)
        .or_else(|| host_of(&article.url))
        .unwrap_or_default();
    if host.is_empty() {
        return false;
    }
    cfg.blocked_domains.iter().any(|raw| {
        let blocked = raw.trim().trim_start_matches('.').to_lowercase();
        !blocked.is_empty() && (host == blocked || host.ends_with(&format!(".{blocked}")))
    })
}

/// Lowercased host of a URL, `www.` stripped.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()?;
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host.trim_start_matches("www.").to_string())
    }
}

/// True when the title reads like a link roundup / release note / sponsor post (§3.5).
pub fn looks_like_roundup(title: &str) -> bool {
    let lower = title.to_lowercase();
    PENALTY_TITLE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Long-form bonus: zero below [`LONGFORM_FLOOR_WORDS`], saturating at
/// [`LONGFORM_SATURATION_WORDS`], with a concave curve so that the jump from a
/// 400-word note to a 1,200-word piece matters more than 2,000 → 2,500 (§3.5).
pub fn longform_points(word_count: i64) -> f64 {
    let span = (LONGFORM_SATURATION_WORDS - LONGFORM_FLOOR_WORDS) as f64;
    let over = (word_count - LONGFORM_FLOOR_WORDS).max(0) as f64;
    MAX_LONGFORM_POINTS * (over / span).min(1.0).powf(0.65)
}

/// Social proof, log-scaled a second time so that a viral story cannot swamp the
/// long-form preference (§3.4, §3.5).
pub fn social_points(social_score: f64) -> f64 {
    if social_score <= 0.0 {
        return 0.0;
    }
    MAX_SOCIAL_POINTS * (social_score / SOCIAL_SATURATION).min(1.0).sqrt()
}

/// Text-only heuristic used by personalized ranking (§9.3).
pub fn text_heuristic(article: &Article) -> f64 {
    longform_points(article.word_count)
        - excerpt_only_penalty(article)
        - roundup_penalty(&article.title)
}

pub fn excerpt_only_penalty(article: &Article) -> f64 {
    if article.excerpt_only {
        EXCERPT_ONLY_PENALTY
    } else {
        0.0
    }
}

pub fn roundup_penalty(title: &str) -> f64 {
    if looks_like_roundup(title) {
        ROUNDUP_TITLE_PENALTY
    } else {
        0.0
    }
}

/// Score one article 0–100 from word count, social proof, source signals,
/// and the excerpt/roundup/blocklist penalties (§3.5).
pub fn score_article(article: &Article, _ctx: &PrefilterContext, cfg: &Config) -> f64 {
    if is_blocked(article, &cfg.curation) {
        return 0.0;
    }
    let mut score = longform_points(article.word_count);
    score += social_points(article.social_score());

    if article.came_via(SourceKind::Scour) {
        score += SCOUR_BONUS;
    }
    if article.came_via(SourceKind::HnFrontpage) {
        score += HN_FRONTPAGE_BONUS;
    }

    let extra_feeds = article.sources.len().saturating_sub(1) as f64;
    score += (extra_feeds * 4.0).min(MAX_MULTI_SOURCE_POINTS);

    if article.excerpt_only {
        score -= EXCERPT_ONLY_PENALTY;
    }
    if looks_like_roundup(&article.title) {
        score -= ROUNDUP_TITLE_PENALTY;
    }

    score.clamp(0.0, 100.0)
}

/// Apply [`score_article`] to everything, drop history duplicates, then keep the
/// top `prefilter_keep` plus every auto-include (§3.5).
pub fn run(articles: Vec<Article>, ctx: &PrefilterContext, cfg: &Config) -> Vec<ScoredArticle> {
    let published: HashSet<ArticleId> = ctx.already_published.iter().copied().collect();
    let rejected: HashSet<ArticleId> = ctx.recently_rejected.iter().copied().collect();

    let total = articles.len();
    let (mut dropped_history, mut dropped_blocked) = (0usize, 0usize);
    let mut scored: Vec<ScoredArticle> = Vec::with_capacity(total);

    for article in articles {
        let auto_include = is_auto_include(&article, &cfg.curation);

        // Never print the same story twice, not even from an always-include feed.
        if published.contains(&article.id) {
            dropped_history += 1;
            continue;
        }
        // "Don't re-score churn" (§3.5) — but an always-include feed still gets in.
        if !auto_include && rejected.contains(&article.id) {
            dropped_history += 1;
            continue;
        }
        if !auto_include && is_blocked(&article, &cfg.curation) {
            dropped_blocked += 1;
            continue;
        }

        let prefilter_score = score_article(&article, ctx, cfg);
        let social_score = article.social_score();
        scored.push(ScoredArticle {
            article,
            prefilter_score,
            social_score,
            llm: None,
            auto_include,
        });
    }

    // Descending by score; ties broken by word count then id so the order is
    // deterministic across runs (notes §12).
    sort_by_prefilter(&mut scored);

    let keep = cfg.prefilter_keep.max(cfg.target_article_count);
    let kept: Vec<ScoredArticle> = if scored.len() <= keep {
        scored
    } else {
        let (head, tail) = scored.split_at(keep);
        let mut kept = head.to_vec();
        // Auto-includes below the cut are pulled back in — they can't be dropped.
        kept.extend(tail.iter().filter(|s| s.auto_include).cloned());
        sort_by_prefilter(&mut kept);
        kept
    };

    tracing::info!(
        input = total,
        kept = kept.len(),
        auto_includes = kept.iter().filter(|s| s.auto_include).count(),
        dropped_history,
        dropped_blocked,
        "pre-filter complete"
    );
    kept
}

/// Deterministic ranking: score desc, then longer, then lowest id (notes §12).
pub fn sort_by_prefilter(scored: &mut [ScoredArticle]) {
    scored.sort_by(|a, b| {
        b.prefilter_score
            .partial_cmp(&a.prefilter_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.article.word_count.cmp(&a.article.word_count))
            .then_with(|| a.article.id.cmp(&b.article.id))
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::{ExtractMethod, SocialRef, SocialSource, SourceRef};
    use jiff::Timestamp;

    pub(crate) fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z"
            .parse()
            .expect("static timestamp parses")
    }

    /// A plain 800-word article from feed 7 with no social proof.
    pub(crate) fn article(id: ArticleId, title: &str, word_count: i64) -> Article {
        Article {
            id,
            canonical_url: format!("https://example.com/{id}"),
            title: title.into(),
            best_entry_id: 1000 + id,
            content_html: format!("<p>{}</p>", "word ".repeat(word_count.max(0) as usize)),
            word_count,
            excerpt_only: false,
            image_count: 0,
            sources: vec![SourceRef {
                entry_id: 1000 + id,
                feed_id: 7,
                feed_title: "Some Blog".into(),
                category: Some("Tech".into()),
                kind: SourceKind::Feed,
            }],
            first_seen: ts(),
            url: format!("https://example.com/{id}"),
            author: Some("A. Writer".into()),
            feed_id: 7,
            feed_title: "Some Blog".into(),
            category: Some("Tech".into()),
            published_at: Some(ts()),
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Readability,
        }
    }

    pub(crate) fn with_social(mut a: Article, points: i64, comments: i64) -> Article {
        a.social = vec![SocialRef {
            article_id: a.id,
            source: SocialSource::Hn,
            item_id: Some("1".into()),
            score: points,
            num_comments: comments,
            item_url: Some("https://news.ycombinator.com/item?id=1".into()),
            fetched_at: ts(),
        }];
        a
    }

    pub(crate) fn via(mut a: Article, kind: SourceKind, feed_id: FeedId) -> Article {
        a.sources.push(SourceRef {
            entry_id: a.best_entry_id,
            feed_id,
            feed_title: format!("{kind:?} feed"),
            category: None,
            kind,
        });
        a
    }

    fn cfg() -> Config {
        Config {
            prefilter_keep: 3,
            target_article_count: 2,
            ..Config::default()
        }
    }

    #[test]
    fn longform_curve_is_monotonic_and_bounded() {
        assert_eq!(longform_points(0), 0.0);
        assert_eq!(longform_points(LONGFORM_FLOOR_WORDS), 0.0);
        let mut prev = -1.0;
        for wc in [0, 100, 299, 300, 500, 900, 1500, 2200, 2500, 9000] {
            let pts = longform_points(wc);
            assert!(pts >= prev, "not monotonic at {wc}");
            assert!(pts <= MAX_LONGFORM_POINTS);
            prev = pts;
        }
        assert!((longform_points(2500) - MAX_LONGFORM_POINTS).abs() < 1e-9);
        assert!((longform_points(50_000) - MAX_LONGFORM_POINTS).abs() < 1e-9);
    }

    #[test]
    fn social_curve_is_monotonic_and_bounded() {
        let mut prev = -1.0;
        for s in [0.0, 0.5, 1.0, 2.0, 4.0, 6.0, 20.0] {
            let pts = social_points(s);
            assert!(pts >= prev);
            assert!(pts <= MAX_SOCIAL_POINTS);
            prev = pts;
        }
        assert_eq!(social_points(0.0), 0.0);
        assert!((social_points(6.0) - MAX_SOCIAL_POINTS).abs() < 1e-9);
    }

    #[test]
    fn score_rises_with_length_and_social_proof() {
        let (ctx, cfg) = (PrefilterContext::default(), cfg());
        let short = score_article(&article(1, "A thought", 200), &ctx, &cfg);
        let medium = score_article(&article(2, "An essay", 1200), &ctx, &cfg);
        let long = score_article(&article(3, "A treatise", 3000), &ctx, &cfg);
        assert!(short < medium, "{short} !< {medium}");
        assert!(medium < long, "{medium} !< {long}");

        let quiet = score_article(&article(4, "An essay", 1200), &ctx, &cfg);
        let loud = score_article(
            &with_social(article(5, "An essay", 1200), 400, 250),
            &ctx,
            &cfg,
        );
        assert!(loud > quiet);
        assert!(loud <= 100.0);
    }

    #[test]
    fn source_bonuses_and_penalties_apply() {
        let (ctx, cfg) = (PrefilterContext::default(), cfg());
        // Long enough that the penalties do not run into the 0 floor.
        let plain = score_article(&article(1, "Deep dive", 3000), &ctx, &cfg);
        assert!(plain > EXCERPT_ONLY_PENALTY);

        let scoured = score_article(
            &via(article(2, "Deep dive", 3000), SourceKind::Scour, 42),
            &ctx,
            &cfg,
        );
        // Scour bonus + one extra feed in the cluster.
        assert!(scoured > plain + SCOUR_BONUS - 0.001);

        let mut excerpt = article(3, "Deep dive", 3000);
        excerpt.excerpt_only = true;
        assert!(
            (score_article(&excerpt, &ctx, &cfg) - (plain - EXCERPT_ONLY_PENALTY)).abs() < 1e-9
        );

        let roundup = article(4, "This Week in Rust #612", 3000);
        assert!(looks_like_roundup(&roundup.title));
        assert!(
            (score_article(&roundup, &ctx, &cfg) - (plain - ROUNDUP_TITLE_PENALTY)).abs() < 1e-9
        );
    }

    #[test]
    fn blocked_domains_and_auto_includes_match_urls_and_ids() {
        let mut cfg = cfg();
        cfg.curation.blocked_domains = vec!["spam.example".into()];
        cfg.curation.always_include_feeds = vec!["99".into(), "tyler.blog".into()];

        let mut blocked = article(1, "Buy now", 1200);
        blocked.canonical_url = "https://news.spam.example/post".into();
        blocked.url.clone_from(&blocked.canonical_url);
        assert!(is_blocked(&blocked, &cfg.curation));
        assert_eq!(
            score_article(&blocked, &PrefilterContext::default(), &cfg),
            0.0
        );

        let mut by_url = article(2, "A rare post", 900);
        by_url.url = "https://tyler.blog/2026/rare".into();
        assert!(is_auto_include(&by_url, &cfg.curation));

        let mut by_id = article(3, "Another rare post", 900);
        by_id.feed_id = 99;
        assert!(is_auto_include(&by_id, &cfg.curation));

        assert!(!is_auto_include(&article(4, "Normal", 900), &cfg.curation));
    }

    #[test]
    fn keeps_top_n_plus_auto_includes_and_drops_history() {
        let mut cfg = cfg();
        cfg.prefilter_keep = 2;
        cfg.curation.always_include_feeds = vec!["99".into()];

        let mut auto = article(5, "A short personal note", 120);
        auto.feed_id = 99;

        let articles = vec![
            article(1, "Long treatise", 4000),
            article(2, "Medium essay", 1500),
            article(3, "Shorter piece", 700),
            article(4, "Already printed", 5000),
            auto,
            article(6, "Rejected yesterday", 3000),
        ];
        let ctx = PrefilterContext {
            already_published: vec![4],
            recently_rejected: vec![6],
        };

        let kept = run(articles, &ctx, &cfg);
        let ids: Vec<ArticleId> = kept.iter().map(|s| s.article.id).collect();
        assert!(!ids.contains(&4), "previously published must be dropped");
        assert!(!ids.contains(&6), "recently rejected must be dropped");
        assert!(ids.contains(&5), "auto-include survives below the cut");
        assert!(ids.contains(&1) && ids.contains(&2));
        assert!(!ids.contains(&3), "cut at prefilter_keep");
        assert_eq!(kept.len(), 3); // 2 kept + 1 auto-include

        // Sorted by score, descending.
        for pair in kept.windows(2) {
            assert!(pair[0].prefilter_score >= pair[1].prefilter_score);
        }
        assert!(
            kept.iter()
                .find(|s| s.article.id == 5)
                .is_some_and(|s| s.auto_include)
        );
    }

    #[test]
    fn auto_include_survives_the_recently_rejected_list_but_not_republication() {
        let mut cfg = cfg();
        cfg.curation.always_include_feeds = vec!["99".into()];
        let mut a = article(1, "Personal note", 200);
        a.feed_id = 99;
        let mut b = article(2, "Personal note two", 200);
        b.feed_id = 99;

        let ctx = PrefilterContext {
            recently_rejected: vec![1],
            already_published: vec![2],
        };
        let kept = run(vec![a, b], &ctx, &cfg);
        let ids: Vec<ArticleId> = kept.iter().map(|s| s.article.id).collect();
        assert_eq!(ids, vec![1]);
    }

    #[tokio::test]
    async fn context_loads_history_from_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::db::Db::open_and_migrate(&dir.path().join("t.db"))
            .await
            .expect("db");
        let date: jiff::civil::Date = "2026-08-15".parse().expect("date");

        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
                 (42, 'https://example.com/42', 'Printed', '2026-08-14T00:00:00Z'),
                 (43, 'https://example.com/43', 'Rejected', '2026-08-14T00:00:00Z'),
                 (44, 'https://example.com/44', 'Ancient', '2020-01-01T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .expect("articles");
        db.upsert_issue(
            "2026-08-14".parse().expect("date"),
            1,
            ts(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("issue");
        sqlx::query(
            "INSERT INTO issue_articles (issue_date, article_id, section, position, is_lead)
             VALUES ('2026-08-14', 42, 'Top Stories', 1, 0)",
        )
        .execute(db.pool())
        .await
        .expect("issue article");
        sqlx::query(
            "INSERT INTO scores (article_id, run_date, llm_score) VALUES (43, '2026-08-14', 1.5),
                                                                        (44, '2020-01-01', 1.0)",
        )
        .execute(db.pool())
        .await
        .expect("scores");

        let ctx = PrefilterContext::load(&db, date).await.expect("context");
        assert_eq!(ctx.already_published, vec![42]);
        assert_eq!(ctx.recently_rejected, vec![43], "old rejects age out");
    }
}
