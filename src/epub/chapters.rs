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
use crate::types::{
    Article, BehindThePaper, Edition, ImageAsset, Issue, NearMiss, Pick, SocialRef, Vote,
    WORLD_BRIEFING_SECTION,
};
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
    why: Option<String>,
    understanding: Option<String>,
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
}

struct RatingLinks {
    loved_url: String,
    good_url: String,
    not_for_me_url: String,
    slop_url: String,
}

#[derive(Template)]
#[template(path = "chapter.xhtml", escape = "html")]
struct ArticleChapter {
    title: String,
    article_title: String,
    byline: Option<String>,
    meta_line: String,
    social_line: Option<String>,
    understanding: Option<String>,
    why: Option<String>,
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
#[template(path = "behind.xhtml", escape = "html")]
struct BehindChapter {
    title: String,
    summary_line: String,
    admitted_line: String,
    learned_line: String,
    near_misses: Vec<String>,
    models_line: String,
}

#[derive(Template)]
#[template(path = "colophon.xhtml", escape = "html")]
struct ColophonChapter {
    title: String,
    issue_number: i64,
    display_date: String,
    generated_at: String,
    bulk_model: String,
    editor_model: String,
    summaries_model: String,
    provider_costs: Vec<ProviderCostLine>,
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

struct ProviderCostLine {
    provider: String,
    cost: String,
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

fn facet_label(token: &str) -> Option<String> {
    let label = match token {
        "software_engineering" => "Software engineering",
        "ai_ml" => "AI & ML",
        "science_space" => "Science & space",
        "culture_arts" => "Culture & arts",
        "books_writing" => "Books & writing",
        "games" => "Games",
        "hardware" => "Hardware",
        "internet_web" => "Internet & web",
        "business_economics" => "Business & economics",
        "politics_policy" => "Politics & policy",
        "boston_new_england" => "Boston & New England",
        "outdoors_lifestyle" => "Outdoors & lifestyle",
        "history" => "History",
        "reported_news" => "reported news",
        "analysis_essay" => "analysis essay",
        "how_to_technical" => "how-to",
        "first_hand_account" => "first-hand account",
        "announcement_roundup" => "announcement",
        "code_repository" => "code repository",
        "documentation_reference" => "documentation",
        "tool_or_product_page" => "product page",
        "discussion_thread" => "discussion thread",
        "paper_or_report" => "paper or report",
        "interview_or_transcript" => "interview",
        "video_or_podcast" => "video or podcast",
        "fiction_or_humor" => "fiction or humor",
        "brief" => "brief",
        "standard" => "standard depth",
        "deep" => "in depth",
        "nontechnical" => "non-technical",
        "light" => "lightly technical",
        "intermediate" => "moderately technical",
        "advanced" => "highly technical",
        "other" => return None,
        unknown => return Some(unknown.replace('_', " ")),
    };
    Some(label.to_string())
}

/// One muted line saying what the pipeline understood about an article: the
/// deep-assessment facets, the extracted topics and the best-matching reader
/// interests. `None` when there is nothing to say (no deep read, no interests).
pub fn understanding_line(pick: &Pick) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(deep) = &pick.llm {
        let facets = &deep.facets;
        for token in [
            facets.topic_group.as_deref(),
            facets.format.as_deref(),
            facets.depth.as_deref(),
            facets.technicality.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(label) = facet_label(token).filter(|label| !label.is_empty()) {
                parts.push(label);
            }
        }
        if let Some(topics) = &facets.specific_topics {
            let topics = topics
                .iter()
                .map(|topic| topic.trim())
                .filter(|topic| !topic.is_empty())
                .collect::<Vec<_>>();
            if !topics.is_empty() {
                parts.push(format!("Topics: {}", topics.join(", ")));
            }
        }
    }
    if !pick.top_interests.is_empty() {
        parts.push(format!("Interests: {}", pick.top_interests.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{00b7} "))
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

/// The Brief (§14.2) under the masthead, plus the issue stats line (§3.10).
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
        title: "The Brief".into(),
        display_date: issue.meta.display_date.clone(),
        issue_number: issue.meta.issue_number,
        stats_line: issue.meta.stats_line(),
        body_html,
    };
    Ok(Chapter {
        id: "front".into(),
        href: "front.xhtml".into(),
        title: "The Brief".into(),
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

fn source_line(article: &Article) -> String {
    match article.publication_label() {
        Some(publication) => format!("{} · {publication}", article.feed_title),
        None => article.feed_title.clone(),
    }
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
                source: source_line(&pick.article),
                reading_minutes: pick.article.reading_minutes(),
                summary: summary_for(issue, pick).unwrap_or_default().to_string(),
                why: pick.why.clone(),
                understanding: understanding_line(pick),
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
                why: None,
                understanding: None,
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

/// A section title page: the name only (§14.2 removed the LLM intros).
pub fn render_section_page(name: &str) -> Result<Chapter, EpubError> {
    let tpl = SectionPage {
        title: name.to_string(),
        name: name.to_string(),
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
    let mut meta_parts = vec![source_line(article)];
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
            slop_url: rating_url(public_url, secret, issue.meta.date, article.id, Vote::Slop),
        }),
        _ => None,
    };

    let tpl = ArticleChapter {
        title: article.title.clone(),
        article_title: article.title.clone(),
        byline: article.author.as_ref().map(|a| format!("By {a}")),
        meta_line: meta_parts.join(" \u{00b7} "),
        social_line: social_line(&article.social),
        understanding: understanding_line(pick),
        why: pick.why.clone(),
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

/// "1,465" — thousands separators for the counts in Behind the paper.
fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// `Considered 412 articles from 1,465 feeds · 398 eligible · …` (§15.1).
pub fn behind_summary_line(b: &BehindThePaper) -> String {
    format!(
        "Considered {} articles from {} feeds \u{00b7} {} eligible \u{00b7} {} triaged \u{00b7} {} read closely \u{00b7} {} shortlisted \u{00b7} {} selected.",
        thousands(b.considered),
        thousands(b.feeds_seen),
        thousands(b.eligible),
        thousands(b.triaged),
        thousands(b.read_closely),
        thousands(b.shortlisted),
        thousands(b.selected),
    )
}

/// `Admitted via: triage 60 · interests 20 · your ratings 12 · exploration 5 · blend 23.`
pub fn behind_admitted_line(b: &BehindThePaper) -> String {
    let count = |name: &str| b.admitted_by.get(name).copied().unwrap_or(0);
    let mut parts = vec![
        format!("triage {}", count("triage")),
        format!("interests {}", count("interest")),
        format!("your ratings {}", count("knn")),
        format!("exploration {}", count("exploration")),
        format!("blend {}", count("blend")),
    ];
    if count("auto_include") > 0 {
        parts.push(format!("always-include {}", count("auto_include")));
    }
    format!("Admitted via: {}.", parts.join(" \u{00b7} "))
}

/// `Learned signals: 14 rated articles with embeddings (neighbour signal at 35%); feed affinity off.`
pub fn behind_learned_line(b: &BehindThePaper) -> String {
    let percent = |gate: f64| (gate.clamp(0.0, 1.0) * 100.0).round() as i64;
    let neighbour = if b.knn_gate > 0.0 {
        format!("neighbour signal at {}%", percent(b.knn_gate))
    } else {
        "neighbour signal off".to_string()
    };
    let feed = if b.feed_gate > 0.0 {
        format!("feed affinity at {}%", percent(b.feed_gate))
    } else {
        "feed affinity off".to_string()
    };
    format!(
        "Learned signals: {} rated articles with embeddings ({neighbour}); {feed}.",
        thousands(b.rated_with_embeddings)
    )
}

/// `<title> — <feed> · quality 8.0 · fit 6.5 · shortlisted, not selected`.
pub fn behind_near_miss_line(miss: &NearMiss) -> String {
    let mut parts = vec![format!("{} \u{2014} {}", miss.title, miss.feed_title)];
    if let Some(quality) = miss.quality {
        parts.push(format!("quality {quality:.1}"));
    }
    if let Some(fit) = miss.fit {
        parts.push(format!("fit {fit:.1}"));
    }
    parts.push(match &miss.reason {
        Some(reason) => format!("{}, {}", miss.stage, reason.replace('_', " ")),
        None => miss.stage.clone(),
    });
    parts.join(" \u{00b7} ")
}

/// `Models: triage and assessment … · editor and summaries … · embeddings ….
/// Cost $0.81. Generation 23 min.`
pub fn behind_models_line(b: &BehindThePaper) -> String {
    let editorial = if b.models.summaries == b.models.editor {
        format!("editor and summaries {}", b.models.editor)
    } else {
        format!(
            "editor {} \u{00b7} summaries {}",
            b.models.editor, b.models.summaries
        )
    };
    let generation = if b.generation_secs < 60 {
        format!("{} s", b.generation_secs.max(0))
    } else {
        format!("{} min", (b.generation_secs as f64 / 60.0).round() as i64)
    };
    format!(
        "Models: triage and assessment {} \u{00b7} {editorial} \u{00b7} embeddings {}. Cost ${:.2}. Generation {generation}.",
        b.models.bulk, b.embedding_model, b.cost_usd
    )
}

/// "Behind the paper": the run's counts, admission mix, learned-signal state,
/// near misses and models (§15.1). Same text in both editions; no links.
pub fn render_behind_the_paper(issue: &Issue) -> Result<Chapter, EpubError> {
    let behind = &issue.behind;
    let tpl = BehindChapter {
        title: "Behind the paper".into(),
        summary_line: behind_summary_line(behind),
        admitted_line: behind_admitted_line(behind),
        learned_line: behind_learned_line(behind),
        near_misses: behind
            .near_misses
            .iter()
            .map(behind_near_miss_line)
            .collect(),
        models_line: behind_models_line(behind),
    };
    Ok(Chapter {
        id: "behind".into(),
        href: "behind.xhtml".into(),
        title: "Behind the paper".into(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

/// Colophon: generation timestamp, models used, token cost, feed counts (§3.10).
pub fn render_colophon(issue: &Issue) -> Result<Chapter, EpubError> {
    let colophon = &issue.colophon;
    let provider_costs = colophon
        .provider_costs
        .iter()
        .map(|(provider, cost)| ProviderCostLine {
            provider: provider.clone(),
            cost: format!("${cost:.4}"),
        })
        .collect();
    let tpl = ColophonChapter {
        title: "Colophon".into(),
        issue_number: issue.meta.issue_number,
        display_date: issue.meta.display_date.clone(),
        generated_at: issue.meta.generated_at.to_string(),
        bulk_model: colophon.models.bulk.clone(),
        editor_model: colophon.models.editor.clone(),
        summaries_model: colophon.models.summaries.clone(),
        provider_costs,
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
    fn understanding_line_includes_facets_topics_and_interests() {
        let issue = issue();
        assert_eq!(
            understanding_line(&issue.lineup.picks[0]).as_deref(),
            Some(
                "Software engineering · analysis essay · in depth · highly technical · Topics: copy-on-write, ZFS · Interests: Filesystems, Rust"
            )
        );
    }

    #[test]
    fn understanding_line_is_none_without_a_deep_read_or_interests() {
        let issue = issue();
        assert!(understanding_line(&issue.lineup.picks[1]).is_none());
    }

    #[test]
    fn understanding_line_can_contain_only_interests() {
        let issue = issue();
        let mut pick = issue.lineup.picks[1].clone();
        pick.top_interests = vec!["Rust".into()];
        assert_eq!(
            understanding_line(&pick).as_deref(),
            Some("Interests: Rust")
        );
    }

    #[test]
    fn understanding_line_skips_other_topic_group() {
        let issue = issue();
        let mut pick = issue.lineup.picks[0].clone();
        pick.llm.as_mut().unwrap().facets = crate::types::Facets {
            topic_group: Some("other".into()),
            ..crate::types::Facets::default()
        };
        pick.top_interests.clear();
        assert!(understanding_line(&pick).is_none());
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
        assert!(chapter.xhtml.contains("The Brief"));
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
        // The lead's understanding line is on the index; the second pick has none.
        assert_eq!(
            chapter.xhtml.matches("class=\"index-understood\"").count(),
            1
        );
        assert!(chapter.xhtml.contains("Interests: Filesystems, Rust"));
        // Titles are escaped (askama emits numeric references), never injected raw.
        assert!(chapter.xhtml.contains("A Niche Delight &#38; Other Tales"));
        assert_xml_ok(&chapter.xhtml);
    }

    #[test]
    fn article_sources_include_distinct_publications_without_repeating_the_feed() {
        let mut issue = issue();
        issue.lineup.picks[0].article.publication = Some("Example Journal".into());
        issue.lineup.picks[1].article.publication = Some("Example Feed".into());

        let index = render_in_this_issue(&issue).unwrap();
        assert!(
            index
                .xhtml
                .contains("Example Feed · Example Journal &#183; 6 min read")
        );
        assert!(!index.xhtml.contains("Example Feed · Example Feed"));

        let article = render_article(
            &issue,
            &issue.lineup.picks[0],
            &[],
            Edition::Standard,
            "https://daily.hallada.net",
            None,
        )
        .unwrap();
        assert!(article.xhtml.contains("Example Feed · Example Journal"));

        let matching = render_article(
            &issue,
            &issue.lineup.picks[1],
            &[],
            Edition::Standard,
            "https://daily.hallada.net",
            None,
        )
        .unwrap();
        assert!(matching.xhtml.contains("Example Feed"));
        assert!(!matching.xhtml.contains("Example Feed · Example Feed"));
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
        assert!(chapter.xhtml.contains("class=\"understood\""));
        assert!(chapter.xhtml.contains(
            "Software engineering · analysis essay · in depth · highly technical · Topics: copy-on-write, ZFS · Interests: Filesystems, Rust"
        ));
        let second = render_article(
            &issue,
            &issue.lineup.picks[1],
            &[],
            Edition::Standard,
            "https://daily.hallada.net",
            Some("s3cret"),
        )
        .unwrap();
        assert!(!second.xhtml.contains("class=\"understood\""));
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
    fn behind_the_paper_lines_follow_the_plan_shape() {
        let issue = issue();
        let behind = &issue.behind;
        assert_eq!(
            behind_summary_line(behind),
            "Considered 412 articles from 1,465 feeds \u{00b7} 398 eligible \u{00b7} 398 triaged \u{00b7} 120 read closely \u{00b7} 60 shortlisted \u{00b7} 2 selected."
        );
        assert_eq!(
            behind_admitted_line(behind),
            "Admitted via: triage 60 \u{00b7} interests 20 \u{00b7} your ratings 12 \u{00b7} exploration 5 \u{00b7} blend 23."
        );
        assert_eq!(
            behind_learned_line(behind),
            "Learned signals: 14 rated articles with embeddings (neighbour signal at 35%); feed affinity off."
        );
        assert_eq!(
            behind_near_miss_line(&behind.near_misses[0]),
            "The One That Got Away \u{2014} Example Feed \u{00b7} quality 8.0 \u{00b7} fit 6.5 \u{00b7} shortlisted, not selected"
        );
        assert_eq!(
            behind_near_miss_line(&behind.near_misses[1]),
            "Never Read Closely \u{2014} Other Feed \u{00b7} triaged, not admitted"
        );
        assert_eq!(
            behind_models_line(behind),
            "Models: triage and assessment deepseek-v4-flash \u{00b7} editor and summaries claude-opus-5 \u{00b7} embeddings voyage-4-lite. Cost $0.81. Generation 23 min."
        );
        let chapter = render_behind_the_paper(&issue).unwrap();
        assert_eq!(chapter.id, "behind");
        assert_eq!(chapter.href, "behind.xhtml");
        assert!(chapter.xhtml.contains("Behind the paper"));
        assert!(chapter.xhtml.contains("1,465 feeds"));
        assert!(chapter.xhtml.contains("The One That Got Away"));
        assert!(!chapter.xhtml.contains("<a "), "no links in the chapter");
        assert_xml_ok(&chapter.xhtml);

        // Auto-includes are named only when there were any; a short run is in
        // seconds; a differing summaries model is listed separately.
        let mut short = behind.clone();
        short.admitted_by.insert("auto_include".into(), 2);
        short.generation_secs = 48;
        short.feed_gate = 0.6;
        short.models.summaries = "deepseek-v4-flash".into();
        short.near_misses.clear();
        assert!(behind_admitted_line(&short).ends_with("blend 23 \u{00b7} always-include 2."));
        assert!(behind_learned_line(&short).ends_with("feed affinity at 60%."));
        assert!(
            behind_models_line(&short)
                .contains("editor claude-opus-5 \u{00b7} summaries deepseek-v4-flash")
        );
        assert!(behind_models_line(&short).ends_with("Generation 48 s."));
        let mut issue = issue;
        issue.behind = short;
        let chapter = render_behind_the_paper(&issue).unwrap();
        assert!(chapter.xhtml.contains("None recorded"));
        assert_xml_ok(&chapter.xhtml);
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
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
