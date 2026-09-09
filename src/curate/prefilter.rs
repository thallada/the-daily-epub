//! Hygiene matchers and the text-only heuristic used by personalized ranking.
//!
//! This module no longer gates the candidate pool. Admission lives in
//! `curate::admit`; these helpers remain here because hygiene and cheap signals
//! share them (plan §8.1, §9, §18).

use crate::config::CurationConfig;
use crate::types::{Article, FeedId};

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

pub const LONGFORM_SATURATION_WORDS: i64 = 2500;
pub const LONGFORM_FLOOR_WORDS: i64 = 300;
pub const MAX_LONGFORM_POINTS: f64 = 35.0;
pub const EXCERPT_ONLY_PENALTY: f64 = 20.0;
pub const ROUNDUP_TITLE_PENALTY: f64 = 15.0;

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
            && (article.feed_id == id || article.sources.iter().any(|source| source.feed_id == id))
        {
            return true;
        }
        let needle = needle
            .to_lowercase()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        !needle.is_empty() && (url.contains(&needle) || canonical.contains(&needle))
    })
}

pub fn is_blocked(article: &Article, cfg: &CurationConfig) -> bool {
    let host = host_of(&article.canonical_url)
        .or_else(|| host_of(&article.url))
        .unwrap_or_default();
    cfg.blocked_domains.iter().any(|raw| {
        let blocked = raw.trim().trim_start_matches('.').to_lowercase();
        !blocked.is_empty() && (host == blocked || host.ends_with(&format!(".{blocked}")))
    })
}

fn host_of(url: &str) -> Option<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()?;
    let host = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
    let host = host.split_once(':').map(|(host, _)| host).unwrap_or(host);
    let host = host.trim().to_lowercase();
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_string())
}

pub fn looks_like_roundup(title: &str) -> bool {
    let lower = title.to_lowercase();
    PENALTY_TITLE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

pub fn longform_points(word_count: i64) -> f64 {
    let span = (LONGFORM_SATURATION_WORDS - LONGFORM_FLOOR_WORDS) as f64;
    let over = (word_count - LONGFORM_FLOOR_WORDS).max(0) as f64;
    MAX_LONGFORM_POINTS * (over / span).min(1.0).powf(0.65)
}

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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::{ArticleId, ExtractMethod, SocialRef, SocialSource, SourceKind, SourceRef};
    use jiff::Timestamp;

    pub(crate) fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z"
            .parse()
            .expect("static timestamp parses")
    }

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
            publication: None,
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

    pub(crate) fn with_social(mut article: Article, points: i64, comments: i64) -> Article {
        article.social = vec![SocialRef {
            article_id: article.id,
            source: SocialSource::Hn,
            item_id: Some("1".into()),
            score: points,
            num_comments: comments,
            item_url: None,
            fetched_at: ts(),
        }];
        article
    }

    #[test]
    fn text_heuristic_has_only_text_terms() {
        let quiet = article(1, "An essay", 1200);
        let loud = with_social(quiet.clone(), 500, 200);
        assert_eq!(text_heuristic(&quiet), text_heuristic(&loud));
        assert!(text_heuristic(&article(2, "This Week in Rust", 1200)) < text_heuristic(&quiet));
    }

    #[test]
    fn blocked_and_auto_include_match() {
        let cfg = CurationConfig {
            blocked_domains: vec!["spam.example".into()],
            always_include_feeds: vec!["99".into(), "tyler.blog".into()],
            ..CurationConfig::default()
        };
        let mut blocked = article(1, "spam", 100);
        blocked.url = "https://news.spam.example/a".into();
        blocked.canonical_url.clone_from(&blocked.url);
        assert!(is_blocked(&blocked, &cfg));
        let mut auto = article(2, "post", 100);
        auto.feed_id = 99;
        assert!(is_auto_include(&auto, &cfg));
    }
}
