//! A synthetic, fully offline [`crate::types::Issue`] for tests.
//!
//! Lives here rather than inside a `#[cfg(test)]` block because the integration
//! tests in `tests/` can only see the public API.

use std::collections::BTreeMap;

use jiff::Timestamp;

use crate::types::*;

pub fn timestamp() -> Timestamp {
    "2026-08-15T05:30:00Z".parse().expect("fixed timestamp")
}

pub fn article(id: ArticleId, entry_id: EntryId, title: &str) -> Article {
    Article {
        id,
        canonical_url: format!("https://example.com/{entry_id}"),
        title: title.to_string(),
        best_entry_id: entry_id,
        content_html: format!(
            "<p>Body of <em>{title}</em> with an image.</p><img src=\"https://img.example/{entry_id}.png\" alt=\"A chart of the daily figures\"><p>More words &amp; things.</p>"
        ),
        word_count: 1200,
        excerpt_only: false,
        image_count: 1,
        sources: vec![SourceRef {
            entry_id,
            feed_id: 7,
            feed_title: "Example Feed".into(),
            category: Some("Tech".into()),
            kind: SourceKind::Feed,
        }],
        first_seen: timestamp(),
        url: format!("https://example.com/{entry_id}"),
        author: Some("A. Writer".into()),
        feed_id: 7,
        feed_title: "Example Feed".into(),
        category: Some("Tech".into()),
        published_at: Some(timestamp()),
        comments_url: None,
        image_urls: vec![format!("https://img.example/{entry_id}.png")],
        social: vec![SocialRef {
            article_id: id,
            source: SocialSource::Hn,
            item_id: Some("40100000".into()),
            score: 342,
            num_comments: 210,
            item_url: Some("https://news.ycombinator.com/item?id=40100000".into()),
            fetched_at: timestamp(),
        }],
        extract_method: ExtractMethod::Readability,
    }
}

pub fn discussion(article_id: ArticleId, entry_id: EntryId) -> Discussion {
    Discussion {
        article_id,
        chapter_id: format!("disc-{entry_id}"),
        threads: vec![CommentThread {
            source: SocialSource::Hn,
            item_url: "https://news.ycombinator.com/item?id=40100000".into(),
            total_comments: 210,
            comments: vec![Comment {
                author: "alice".into(),
                points: Some(61),
                text_html: "<p>The write path is the interesting part.</p>".into(),
                depth: 0,
                children: vec![Comment {
                    author: "bob".into(),
                    points: Some(24),
                    text_html: "<p>Agreed.</p>".into(),
                    depth: 1,
                    children: vec![],
                }],
            }],
        }],
    }
}

/// A synthetic two-article issue, one of them carrying a discussion.
pub fn issue() -> Issue {
    let lead = Pick {
        article: article(1, 1001, "The Lead Story"),
        section: "Top Stories".into(),
        position: 0,
        is_lead: true,
        why: Some("The systems story with enough operational detail to matter".into()),
        summary: Some("What it argues, and why it is worth the time.".into()),
        llm: None,
        discussion: Some(discussion(1, 1001)),
    };
    let second = Pick {
        article: article(2, 1002, "A Niche Delight & Other Tales"),
        section: "Niche Corner".into(),
        position: 0,
        is_lead: false,
        why: Some("A small-scene delight outside the usual technical orbit".into()),
        summary: None,
        llm: None,
        discussion: None,
    };
    let mut summaries = BTreeMap::new();
    summaries.insert(2, "A short abstract for the second piece.".to_string());

    Issue {
        meta: IssueMeta {
            date: "2026-08-15".parse().expect("fixed date"),
            issue_number: 42,
            generated_at: timestamp(),
            display_date: "Friday, August 15, 2026".into(),
            article_count: 2,
            section_count: 2,
            total_words: 2400,
            reading_minutes: 11,
        },
        lineup: Lineup {
            date: "2026-08-15".parse().expect("fixed date"),
            picks: vec![lead, second],
            section_order: vec!["Top Stories".into(), "Niche Corner".into()],
        },
        editorial: Editorial {
            front_page_html: "<p>Two stories today, both worth your coffee.</p>".into(),
            summaries,
        },
        world_briefing: Some(WorldBriefing {
            date: "2026-08-15".parse().expect("fixed date"),
            source_url: "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_15".into(),
            overview: Some("A concise view of the day.".into()),
            sections: vec![WorldBriefingSection {
                title: "Top Stories".into(),
                events: vec![WorldEvent {
                    id: "s1-e1".into(),
                    source_text: "Something happened somewhere.".into(),
                    links: vec![],
                    children: vec![],
                    summary: Some("The event in context.".into()),
                }],
            }],
        }),
        colophon: Colophon {
            provider_costs: BTreeMap::from([
                ("deepseek".into(), 0.0231),
                ("anthropic".into(), 0.05),
            ]),
            models: Models {
                bulk: "deepseek-v4-flash".into(),
                editor: "claude-opus-5".into(),
                summaries: "claude-opus-5".into(),
            },
            entries_fetched: 431,
            feeds_seen: 92,
            candidates: 120,
            cost_usd: 0.0731,
            generator_version: "daily-epub 0.1.0".into(),
        },
    }
}

/// A crude XHTML well-formedness check for rendered chapters.
///
/// The real proof is `epubcheck`, which cannot run in a unit test; this catches
/// the mistakes that actually happen — an unclosed void element or a raw
/// `&nbsp;`, both of which make an EPUB3 content document unparseable.
pub fn assert_xml_ok(xhtml: &str) {
    // A crude well-formedness check: the document parses as XML only if every
    // tag is closed, so compare open/close counts for the elements we emit.
    assert!(xhtml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    assert!(xhtml.contains("xmlns=\"http://www.w3.org/1999/xhtml\""));
    assert!(xhtml.trim_end().ends_with("</html>"));
    for tag in ["html", "head", "body", "div", "p"] {
        let opens = xhtml.matches(&format!("<{tag}")).count();
        let closes = xhtml.matches(&format!("</{tag}>")).count();
        assert_eq!(opens, closes, "unbalanced <{tag}> in\n{xhtml}");
    }
    assert!(!xhtml.contains("&nbsp;"));
    for void in ["<br>", "<hr>", "<img "] {
        if void == "<img " {
            for (i, _) in xhtml.match_indices("<img ") {
                let tail = &xhtml[i..];
                let end = tail.find('>').unwrap_or(0);
                assert!(tail[..end].ends_with('/'), "unclosed <img> in\n{xhtml}");
            }
        } else {
            assert!(!xhtml.contains(void), "unclosed {void} in\n{xhtml}");
        }
    }
}
