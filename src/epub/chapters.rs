//! Rendering one chapter of XHTML at a time (spec §3.10).
//!
//! Each function here turns part of an [`Issue`] into a [`Chapter`]: the front
//! page, the index, section title pages, articles, discussions, the world
//! briefing and the colophon. [`super::build::render_all`] puts them in order;
//! [`super::build::assemble`] zips them.

use askama::Template;

use crate::comments;
use crate::html::{text_escape, to_xhtml};
use crate::images;
use crate::types::{Edition, ImageAsset, Issue, Pick, SocialRef, Vote, WORLD_BRIEFING_SECTION};
use crate::world;

use super::EpubError;
use super::build::Chapter;

/// Characters of the hex HMAC kept in rating links (§3.9).
pub const TOKEN_LEN: usize = crate::auth::TOKEN_LEN;

// One implementation, shared with the verifying side in [`crate::server`]:
// see [`crate::auth`] for the formula and the pinned test vector (§3.9).
pub use crate::auth::{rating_message, rating_token, rating_url};

#[derive(Template)]
#[template(path = "front_page.xhtml", escape = "html")]
struct FrontPage {
    title: String,
    display_date: String,
    issue_number: i64,
    stats_line: String,
    body_html: String,
}

struct IndexEntry {
    href: String,
    title: String,
    source: String,
    reading_minutes: i64,
    summary: String,
}

struct IndexSection {
    name: String,
    entries: Vec<IndexEntry>,
}

#[derive(Template)]
#[template(path = "in_this_issue.xhtml", escape = "html")]
struct InThisIssue {
    title: String,
    stats_line: String,
    sections: Vec<IndexSection>,
}

#[derive(Template)]
#[template(path = "section.xhtml", escape = "html")]
struct SectionPage {
    title: String,
    name: String,
    intro: Option<String>,
}

struct RatingLinks {
    loved_url: String,
    good_url: String,
    not_for_me_url: String,
}

#[derive(Template)]
#[template(path = "chapter.xhtml", escape = "html")]
struct ArticleChapter {
    title: String,
    article_title: String,
    byline: Option<String>,
    meta_line: String,
    social_line: Option<String>,
    summary: Option<String>,
    excerpt_only: bool,
    body_html: String,
    rating: Option<RatingLinks>,
    read_online_url: String,
    discussion_href: Option<String>,
}

#[derive(Template)]
#[template(path = "discussion.xhtml", escape = "html")]
struct DiscussionChapter {
    title: String,
    heading: String,
    body_html: String,
    article_href: String,
}

#[derive(Template)]
#[template(path = "world_briefing.xhtml", escape = "html")]
struct WorldBriefingChapter {
    title: String,
    display_date: String,
    body_html: String,
}

#[derive(Template)]
#[template(path = "colophon.xhtml", escape = "html")]
struct ColophonChapter {
    title: String,
    issue_number: i64,
    display_date: String,
    generated_at: String,
    model: String,
    entries_fetched: i64,
    feeds_seen: i64,
    candidates: i64,
    article_count: i64,
    section_count: i64,
    total_words: i64,
    reading_line: String,
    cost_usd: String,
    generator_version: String,
}

// ---------------------------------------------------------------------------
// Chapters (§3.10)
// ---------------------------------------------------------------------------

/// Prepare article markup for XHTML: rewrite images, sanitize, self-close voids.
pub fn prepare_body(html: &str, images_: &[ImageAsset]) -> String {
    // Sanitize first: image rewriting emits our own trusted markup (including the
    // `image-placeholder` class, which ammonia would otherwise strip).
    let cleaned = ammonia::clean(html);
    let rewritten = images::rewrite_img_srcs(&cleaned, images_);
    to_xhtml(&rewritten)
}

/// "~2h 15m read" for the colophon (§3.10).
fn reading_line(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("~{h}h {m}m read")
    } else {
        format!("~{m}m read")
    }
}

/// "▲ 342 on HN · 210 comments" (§3.10).
pub fn social_line(social: &[SocialRef]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut comments = 0i64;
    for entry in social {
        if entry.score > 0 {
            parts.push(format!(
                "\u{25b2} {} on {}",
                entry.score,
                entry.source.display_name()
            ));
        }
        comments += entry.num_comments.max(0);
    }
    if comments > 0 {
        let noun = if comments == 1 { "comment" } else { "comments" };
        parts.push(format!("{comments} {noun}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" \u{00b7} "))
    }
}

fn article_href(pick: &Pick) -> String {
    format!("{}.xhtml", pick.article.chapter_id())
}

fn discussion_href(pick: &Pick) -> String {
    format!("disc-{}.xhtml", pick.article.best_entry_id)
}

fn section_href(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("sec-{slug}.xhtml")
}

fn summary_for<'a>(issue: &'a Issue, pick: &'a Pick) -> Option<&'a str> {
    pick.summary
        .as_deref()
        .or_else(|| {
            issue
                .editorial
                .summaries
                .get(&pick.article.id)
                .map(|s| s.as_str())
        })
        .filter(|s| !s.trim().is_empty())
}

fn published_display(pick: &Pick) -> Option<String> {
    pick.article
        .published_at
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::UTC).date().to_string())
}

/// "From the Editor" front page plus the issue stats line (§3.10).
pub fn render_front_page(issue: &Issue) -> Result<Chapter, EpubError> {
    let body = issue.editorial.front_page_html.trim();
    let body_html = if body.is_empty() {
        format!(
            "<p>{} of reading, chosen overnight.</p>",
            text_escape(&issue.meta.stats_line())
        )
    } else {
        to_xhtml(&ammonia::clean(body))
    };
    let tpl = FrontPage {
        title: "From the Editor".into(),
        display_date: issue.meta.display_date.clone(),
        issue_number: issue.meta.issue_number,
        stats_line: issue.meta.stats_line(),
        body_html,
    };
    Ok(Chapter {
        id: "front".into(),
        href: "front.xhtml".into(),
        title: "From the Editor".into(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

/// Section names in issue order, with the reserved world section removed (§3.6).
pub fn section_names(issue: &Issue) -> Vec<String> {
    let mut names: Vec<String> = issue
        .lineup
        .section_order
        .iter()
        .filter(|s| s.as_str() != WORLD_BRIEFING_SECTION)
        .cloned()
        .collect();
    for pick in &issue.lineup.picks {
        if pick.section != WORLD_BRIEFING_SECTION && !names.contains(&pick.section) {
            names.push(pick.section.clone());
        }
    }
    names.retain(|n| issue.lineup.picks.iter().any(|p| &p.section == n));
    names
}

/// "In This Issue": per section, each article's title, source, reading time and
/// summary, linked to its chapter (§3.10).
pub fn render_in_this_issue(issue: &Issue) -> Result<Chapter, EpubError> {
    let mut sections = Vec::new();
    for name in section_names(issue) {
        let entries = issue
            .lineup
            .section_picks(&name)
            .into_iter()
            .map(|pick| IndexEntry {
                href: article_href(pick),
                title: pick.article.title.clone(),
                source: pick.article.feed_title.clone(),
                reading_minutes: pick.article.reading_minutes(),
                summary: summary_for(issue, pick).unwrap_or_default().to_string(),
            })
            .collect();
        sections.push(IndexSection { name, entries });
    }
    if issue.world_briefing.is_some() {
        sections.push(IndexSection {
            name: WORLD_BRIEFING_SECTION.to_string(),
            entries: vec![IndexEntry {
                href: "world.xhtml".into(),
                title: "World Briefing".into(),
                source: "Wikipedia Current Events".into(),
                reading_minutes: 3,
                summary: "The day's events, as recorded by the Current Events portal.".into(),
            }],
        });
    }
    let tpl = InThisIssue {
        title: "In This Issue".into(),
        stats_line: issue.meta.stats_line(),
        sections,
    };
    Ok(Chapter {
        id: "in-this-issue".into(),
        href: "in-this-issue.xhtml".into(),
        title: "In This Issue".into(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

/// A section title page: name + LLM intro (§3.10).
pub fn render_section_page(name: &str, intro: Option<&str>) -> Result<Chapter, EpubError> {
    let tpl = SectionPage {
        title: name.to_string(),
        name: name.to_string(),
        intro: intro
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    Ok(Chapter {
        id: format!("sec-{name}"),
        href: section_href(name),
        title: name.to_string(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

/// One article chapter: header, cleaned body with embedded images, rating footer (§3.10).
pub fn render_article(
    issue: &Issue,
    pick: &Pick,
    images_: &[ImageAsset],
    edition: Edition,
    public_url: &str,
    hmac_secret: Option<&str>,
) -> Result<Chapter, EpubError> {
    let article = &pick.article;
    let mut meta_parts = vec![article.feed_title.clone()];
    if let Some(date) = published_display(pick) {
        meta_parts.push(date);
    }
    meta_parts.push(format!(
        "{} min read \u{00b7} {} words",
        article.reading_minutes(),
        article.word_count
    ));

    // The X4 has no browser, so rating links are pointless there (§7).
    let rating = match (hmac_secret, edition) {
        (Some(secret), Edition::Standard) if !secret.is_empty() => Some(RatingLinks {
            loved_url: rating_url(public_url, secret, issue.meta.date, article.id, Vote::Loved),
            good_url: rating_url(public_url, secret, issue.meta.date, article.id, Vote::Good),
            not_for_me_url: rating_url(
                public_url,
                secret,
                issue.meta.date,
                article.id,
                Vote::NotForMe,
            ),
        }),
        _ => None,
    };

    let tpl = ArticleChapter {
        title: article.title.clone(),
        article_title: article.title.clone(),
        byline: article.author.as_ref().map(|a| format!("By {a}")),
        meta_line: meta_parts.join(" \u{00b7} "),
        social_line: social_line(&article.social),
        summary: summary_for(issue, pick).map(str::to_string),
        excerpt_only: article.excerpt_only,
        body_html: prepare_body(&article.content_html, images_),
        rating,
        read_online_url: article.url.clone(),
        discussion_href: pick.discussion.as_ref().map(|_| discussion_href(pick)),
    };
    Ok(Chapter {
        id: article.chapter_id(),
        href: article_href(pick),
        title: article.title.clone(),
        xhtml: tpl.render()?,
        toc_level: 2,
    })
}

/// The discussion chapter that follows an article when it has comments (§3.7).
pub fn render_discussion(pick: &Pick) -> Result<Option<Chapter>, EpubError> {
    let Some(discussion) = &pick.discussion else {
        return Ok(None);
    };
    if discussion.threads.is_empty() {
        return Ok(None);
    }
    let title = comments::chapter_title(&pick.article.title, discussion);
    let tpl = DiscussionChapter {
        title: title.clone(),
        heading: title.clone(),
        body_html: comments::render_xhtml(discussion, &pick.article.title),
        article_href: article_href(pick),
    };
    Ok(Some(Chapter {
        id: format!("disc-{}", pick.article.best_entry_id),
        href: discussion_href(pick),
        title,
        xhtml: tpl.render()?,
        toc_level: 3,
    }))
}

/// The Wikipedia Current Events section chapter (§3.8).
pub fn render_world_briefing(issue: &Issue) -> Result<Option<Chapter>, EpubError> {
    let Some(briefing) = &issue.world_briefing else {
        return Ok(None);
    };
    // The briefing may cover an earlier day than the issue: the portal page for
    // the issue's own date is still an empty stub at 05:30 (§3.8). Dateline the
    // section with the day it actually reports on, not the masthead date.
    let tpl = WorldBriefingChapter {
        title: WORLD_BRIEFING_SECTION.to_string(),
        display_date: crate::pipeline::display_date(briefing.date),
        body_html: world::render_xhtml(briefing),
    };
    Ok(Some(Chapter {
        id: "world".into(),
        href: "world.xhtml".into(),
        title: WORLD_BRIEFING_SECTION.to_string(),
        xhtml: tpl.render()?,
        toc_level: 1,
    }))
}

/// Colophon: generation timestamp, models used, token cost, feed counts (§3.10).
pub fn render_colophon(issue: &Issue) -> Result<Chapter, EpubError> {
    let colophon = &issue.colophon;
    let tpl = ColophonChapter {
        title: "Colophon".into(),
        issue_number: issue.meta.issue_number,
        display_date: issue.meta.display_date.clone(),
        generated_at: issue.meta.generated_at.to_string(),
        model: if colophon.model.is_empty() {
            "none (heuristic selection)".into()
        } else {
            colophon.model.clone()
        },
        entries_fetched: colophon.entries_fetched,
        feeds_seen: colophon.feeds_seen,
        candidates: colophon.candidates,
        article_count: issue.meta.article_count,
        section_count: issue.meta.section_count,
        total_words: issue.meta.total_words,
        reading_line: reading_line(issue.meta.reading_minutes),
        cost_usd: format!("${:.4}", colophon.cost_usd),
        generator_version: if colophon.generator_version.is_empty() {
            format!("daily-epub {}", env!("CARGO_PKG_VERSION"))
        } else {
            colophon.generator_version.clone()
        },
    };
    Ok(Chapter {
        id: "colophon".into(),
        href: "colophon.xhtml".into(),
        title: "Colophon".into(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::fixtures::{self, assert_xml_ok, issue};
    use crate::types::{SocialRef, Vote};
    use hmac::{Hmac, KeyInit};
    use jiff::civil::Date;
    use sha2::Sha256;

    #[test]
    fn rating_token_matches_the_spec_vector() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(
            rating_message(date, 1234, Vote::Loved),
            "2026-08-15/1234/loved"
        );
        // hex(hmac_sha256("test-secret", "2026-08-15/1234/loved"))[..16]
        let token = rating_token("test-secret", date, 1234, Vote::Loved);
        assert_eq!(token.len(), TOKEN_LEN);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

        // Independently computed reference value.
        use hmac::Mac;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(b"2026-08-15/1234/loved");
        let expected: String = hex::encode(mac.finalize().into_bytes())
            .chars()
            .take(16)
            .collect();
        assert_eq!(token, expected);

        // Different vote, article and secret all change the token.
        assert_ne!(
            token,
            rating_token("test-secret", date, 1234, Vote::NotForMe)
        );
        assert_ne!(token, rating_token("test-secret", date, 1235, Vote::Loved));
        assert_ne!(token, rating_token("other-secret", date, 1234, Vote::Loved));
    }

    /// The EPUB signs the links and `server.rs` verifies them: one formula, or no
    /// rating ever lands. This is the vector `server::tests` pins from its side
    /// (`VECTOR_SECRET` / `VECTOR_TOKEN_UP`) — change one, change both (§3.9).
    #[test]
    fn epub_and_server_share_one_token_vector() {
        let date: Date = "2026-08-15".parse().unwrap();
        let token = rating_token("test-secret", date, 42, Vote::Loved);
        assert_eq!(token, "cece96767d6c5f8a");
        assert!(crate::server::verify_token(
            "test-secret",
            date,
            42,
            Vote::Loved,
            &token
        ));
    }

    #[test]
    fn rating_url_has_the_spec_shape() {
        let date: Date = "2026-08-15".parse().unwrap();
        let url = rating_url(
            "https://daily.hallada.net/",
            "s3cret",
            date,
            99,
            Vote::NotForMe,
        );
        let token = rating_token("s3cret", date, 99, Vote::NotForMe);
        assert_eq!(
            url,
            format!("https://daily.hallada.net/r/2026-08-15/99/down?t={token}")
        );
    }

    #[test]
    fn social_line_matches_the_spec_example() {
        let refs = vec![SocialRef {
            article_id: 1,
            source: crate::types::SocialSource::Hn,
            item_id: None,
            score: 342,
            num_comments: 210,
            item_url: None,
            fetched_at: fixtures::timestamp(),
        }];
        assert_eq!(
            social_line(&refs).as_deref(),
            Some("\u{25b2} 342 on HN \u{00b7} 210 comments")
        );
        assert!(social_line(&[]).is_none());
    }

    #[test]
    fn hrefs_are_deterministic() {
        let issue = issue();
        let pick = &issue.lineup.picks[0];
        assert_eq!(article_href(pick), "art-1001.xhtml");
        assert_eq!(discussion_href(pick), "disc-1001.xhtml");
        assert_eq!(
            section_href("Tech & Engineering"),
            "sec-tech-engineering.xhtml"
        );
    }

    #[test]
    fn front_page_renders_stats_and_editorial() {
        let issue = issue();
        let chapter = render_front_page(&issue).unwrap();
        assert_eq!(chapter.href, "front.xhtml");
        assert!(chapter.xhtml.contains("From the Editor"));
        assert!(chapter.xhtml.contains("2 articles"));
        assert!(chapter.xhtml.contains("both worth your coffee"));
        assert!(chapter.xhtml.contains("No. 42"));
        assert_xml_ok(&chapter.xhtml);
    }

    #[test]
    fn in_this_issue_links_every_pick() {
        let issue = issue();
        let chapter = render_in_this_issue(&issue).unwrap();
        assert!(chapter.xhtml.contains("href=\"art-1001.xhtml\""));
        assert!(chapter.xhtml.contains("href=\"art-1002.xhtml\""));
        assert!(chapter.xhtml.contains("Example Feed"));
        assert!(chapter.xhtml.contains("6 min read"));
        assert!(chapter.xhtml.contains("What it argues"));
        assert!(chapter.xhtml.contains("A short abstract"));
        // Titles are escaped (askama emits numeric references), never injected raw.
        assert!(chapter.xhtml.contains("A Niche Delight &#38; Other Tales"));
        assert_xml_ok(&chapter.xhtml);
    }

    #[test]
    fn article_chapter_has_header_body_and_footer() {
        let issue = issue();
        let pick = &issue.lineup.picks[0];
        let chapter = render_article(
            &issue,
            pick,
            &[],
            Edition::Standard,
            "https://daily.hallada.net",
            Some("s3cret"),
        )
        .unwrap();
        assert_eq!(chapter.id, "art-1001");
        assert_eq!(chapter.toc_level, 2);
        assert!(chapter.xhtml.contains("By A. Writer"));
        assert!(chapter.xhtml.contains("Example Feed"));
        assert!(chapter.xhtml.contains("6 min read"));
        assert!(chapter.xhtml.contains("\u{25b2} 342 on HN"));
        assert!(chapter.xhtml.contains("/r/2026-08-15/1/loved?t="));
        assert!(chapter.xhtml.contains("/r/2026-08-15/1/good?t="));
        assert!(chapter.xhtml.contains("/r/2026-08-15/1/down?t="));
        assert!(chapter.xhtml.contains("Read online"));
        assert!(chapter.xhtml.contains("href=\"disc-1001.xhtml\""));
        // The un-downloaded image degrades to a placeholder.
        assert!(
            chapter
                .xhtml
                .contains("[image: A chart of the daily figures]")
        );
        assert_xml_ok(&chapter.xhtml);
    }

    #[test]
    fn x4_articles_omit_rating_links() {
        let issue = issue();
        let chapter = render_article(
            &issue,
            &issue.lineup.picks[0],
            &[],
            Edition::X4,
            "https://daily.hallada.net",
            Some("s3cret"),
        )
        .unwrap();
        assert!(!chapter.xhtml.contains("/r/2026-08-15/"));
        assert!(chapter.xhtml.contains("Read online"));
    }

    #[test]
    fn discussion_chapter_nests_under_its_article() {
        let issue = issue();
        let chapter = render_discussion(&issue.lineup.picks[0]).unwrap().unwrap();
        assert_eq!(chapter.id, "disc-1001");
        assert_eq!(chapter.toc_level, 3);
        assert!(
            chapter
                .title
                .starts_with("\u{1f4ac} Discussion: The Lead Story")
        );
        assert!(chapter.xhtml.contains("alice"));
        assert!(chapter.xhtml.contains("blockquote class=\"comment\""));
        assert!(chapter.xhtml.contains("href=\"art-1001.xhtml\""));
        assert_xml_ok(&chapter.xhtml);
        assert!(render_discussion(&issue.lineup.picks[1]).unwrap().is_none());
    }

    #[test]
    fn world_and_colophon_chapters_render() {
        let issue = issue();
        let world = render_world_briefing(&issue).unwrap().unwrap();
        assert!(world.xhtml.contains("Something happened somewhere"));
        assert!(world.xhtml.contains("CC BY-SA"));
        assert_xml_ok(&world.xhtml);

        let colophon = render_colophon(&issue).unwrap();
        assert!(colophon.xhtml.contains("deepseek-v4-flash"));
        assert!(colophon.xhtml.contains("431"));
        assert!(colophon.xhtml.contains("$0.0731"));
        assert_xml_ok(&colophon.xhtml);
    }
}
