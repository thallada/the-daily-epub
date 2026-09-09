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
        publication: None,
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
        llm: Some(Deep {
            quality: 9.0,
            fit: 9.0,
            category: Some("Tech & Engineering".into()),
            rationale: "Detailed systems analysis".into(),
            paywalled_guess: false,
            facets: Facets {
                format: Some("analysis_essay".into()),
                depth: Some("deep".into()),
                topic_group: Some("software_engineering".into()),
                technicality: Some("advanced".into()),
                specific_topics: Some(vec!["copy-on-write".into(), "ZFS".into()]),
                ..Facets::default()
            },
            model: "fixture-model".into(),
            prompt_version: 2,
            assessed_at: timestamp(),
        }),
        top_interests: vec!["Filesystems".into(), "Rust".into()],
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
        top_interests: Vec::new(),
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
        behind: BehindThePaper {
            considered: 412,
            feeds_seen: 1465,
            eligible: 398,
            triaged: 398,
            read_closely: 120,
            shortlisted: 60,
            selected: 2,
            admitted_by: BTreeMap::from([
                ("triage".to_string(), 60),
                ("interest".to_string(), 20),
                ("knn".to_string(), 12),
                ("exploration".to_string(), 5),
                ("blend".to_string(), 23),
            ]),
            rated_with_embeddings: 14,
            knn_gate: 0.35,
            feed_gate: 0.0,
            near_misses: vec![
                NearMiss {
                    article_id: 3,
                    title: "The One That Got Away".into(),
                    feed_title: "Example Feed".into(),
                    quality: Some(8.0),
                    fit: Some(6.5),
                    stage: "shortlisted".into(),
                    reason: Some("not_selected".into()),
                },
                NearMiss {
                    article_id: 4,
                    title: "Never Read Closely".into(),
                    feed_title: "Other Feed".into(),
                    quality: None,
                    fit: None,
                    stage: "triaged".into(),
                    reason: Some("not_admitted".into()),
                },
            ],
            models: Models {
                bulk: "deepseek-v4-flash".into(),
                editor: "claude-opus-5".into(),
                summaries: "claude-opus-5".into(),
            },
            embedding_model: "voyage-4-lite".into(),
            cost_usd: 0.81,
            generation_secs: 23 * 60 + 12,
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
