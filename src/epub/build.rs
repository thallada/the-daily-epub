//! `epub-builder` assembly and askama chapter rendering (spec §3.10).
//!
//! Structure: cover → From the Editor → In This Issue → sections (title page,
//! article chapters, discussion chapters) → World Briefing → colophon.

use askama::Template;
use epub_builder::{
    EpubBuilder, EpubContent, EpubVersion, MetadataOpfV3, ReferenceType, ZipLibrary,
};
use jiff::civil::Date;

use crate::comments;
use crate::types::{Edition, ImageAsset, Issue, Pick, SocialRef, Vote, WORLD_BRIEFING_SECTION};
use crate::world;

use super::EpubError;
use super::images;
use super::x4;

/// EPUB3 `belongs-to-collection` name (§3.10).
pub const COLLECTION_NAME: &str = "The Daily EPUB";
/// `id` the collection refinements point at (§3.10).
pub const COLLECTION_ID: &str = "daily-epub-collection";
/// `dc:creator` (§3.10).
pub const CREATOR: &str = "The Daily EPUB";
/// `dc:language` (§3.10).
pub const LANGUAGE: &str = "en";
/// Characters of the hex HMAC kept in rating links (§3.9).
pub const TOKEN_LEN: usize = crate::auth::TOKEN_LEN;
#[derive(Debug, Clone, PartialEq)]
pub struct CoverAsset {
    pub bytes: Vec<u8>,
    pub filename: &'static str,
    pub mime: &'static str,
}

pub fn cover_href(edition: Edition) -> &'static str {
    match edition {
        Edition::Standard => "cover.png",
        Edition::X4 => "cover.jpg",
    }
}

/// One rendered chapter ready to be added to the EPUB (§3.10).
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// Stable id, e.g. `art-30011` / `disc-30011` (notes §12).
    pub id: String,
    /// Path inside the EPUB, e.g. `chapters/art-30011.xhtml`.
    pub href: String,
    pub title: String,
    pub xhtml: String,
    /// 1 for sections, 2 for articles/discussions — nav depth 2 (§3.10).
    pub toc_level: u8,
}

// ---------------------------------------------------------------------------
// Rating links (§3.9)
// ---------------------------------------------------------------------------

// One implementation, shared with the verifying side in [`crate::server`]:
// see [`crate::auth`] for the formula and the pinned test vector (§3.9).
pub use crate::auth::{rating_message, rating_token, rating_url};

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "cover.svg", escape = "html")]
struct CoverSvg {
    width: i64,
    height: i64,
    margin: i64,
    inner_width: i64,
    inner_height: i64,
    border: i64,
    hairline: i64,
    center_x: i64,
    masthead_y: i64,
    masthead_size: i64,
    rule_x1: i64,
    rule_x2: i64,
    rule_y: i64,
    rule2_y: i64,
    weekday: String,
    weekday_y: i64,
    weekday_size: i64,
    long_date: String,
    date_y: i64,
    date_size: i64,
    issue_number: i64,
    issue_y: i64,
    issue_size: i64,
    stats_line: String,
    stats_y: i64,
    stats_size: i64,
    /// Empty for the standard edition — see [`cover_badge`].
    edition_tag: String,
    badge_x: i64,
    badge_y: i64,
    badge_width: i64,
    badge_height: i64,
    badge_radius: i64,
    badge_text_y: i64,
    badge_size: i64,
    footer: String,
    footer_y: i64,
    footer_size: i64,
}

#[derive(Template)]
#[template(path = "cover_page.xhtml", escape = "html")]
struct CoverPage {
    title: String,
    alt: String,
    cover_href: &'static str,
}

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
    up_url: String,
    down_url: String,
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
// Cover (§3.10)
// ---------------------------------------------------------------------------

/// Cover pixel size per edition: 1200×1600 standard, 480×800 X4 (§3.10).
pub fn cover_size(edition: Edition) -> (u32, u32) {
    match edition {
        Edition::Standard => (1200, 1600),
        Edition::X4 => x4::X4_SCREEN,
    }
}

fn reading_line(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("~{h}h {m}m read")
    } else {
        format!("~{m}m read")
    }
}

/// Split "Friday, August 15, 2026" into ("Friday", "August 15, 2026").
fn split_display_date(display: &str) -> (String, String) {
    match display.split_once(", ") {
        Some((weekday, rest)) => (weekday.to_string(), rest.to_string()),
        None => (String::new(), display.to_string()),
    }
}

/// Reversed badge printed on the cover so the two editions are told apart at
/// thumbnail size, where the title is unreadable (§3.10).
///
/// Empty for the standard edition: an unmarked cover is the default, and the
/// presence of the slab is itself the signal.
pub fn cover_badge(edition: Edition) -> &'static str {
    match edition {
        Edition::Standard => "",
        Edition::X4 => "X4 EDITION",
    }
}

fn cover_svg(
    issue: &Issue,
    edition: Edition,
    width: u32,
    height: u32,
) -> Result<String, EpubError> {
    let w = i64::from(width);
    let h = i64::from(height);
    let margin = w / 15;
    let (weekday, long_date) = split_display_date(&issue.meta.display_date);
    // A solid bar between the stats line and the footer: at 100px wide in a
    // library grid the black slab is the only thing still legible.
    let badge_height = h / 16;
    let badge_width = w * 2 / 5;
    let tpl = CoverSvg {
        width: w,
        height: h,
        margin,
        inner_width: w - 2 * margin,
        inner_height: h - 2 * margin,
        border: (w / 300).max(2),
        hairline: (w / 600).max(1),
        center_x: w / 2,
        masthead_y: h * 30 / 100,
        masthead_size: w / 9,
        rule_x1: margin + w / 12,
        rule_x2: w - margin - w / 12,
        rule_y: h * 34 / 100,
        rule2_y: h * 53 / 100,
        weekday,
        weekday_y: h * 42 / 100,
        weekday_size: w / 28,
        long_date,
        date_y: h * 47 / 100,
        date_size: w / 22,
        issue_number: issue.meta.issue_number,
        issue_y: h * 60 / 100,
        issue_size: w / 18,
        stats_line: issue.meta.stats_line(),
        stats_y: h * 66 / 100,
        stats_size: w / 32,
        edition_tag: cover_badge(edition).to_string(),
        badge_x: (w - badge_width) / 2,
        badge_y: h * 73 / 100,
        badge_width,
        badge_height,
        badge_radius: badge_height / 2,
        badge_text_y: h * 73 / 100 + badge_height * 7 / 10,
        badge_size: badge_height * 11 / 20,
        footer: "Assembled overnight \u{00b7} read offline".to_string(),
        footer_y: h - margin - h / 25,
        footer_size: w / 40,
    };
    Ok(tpl.render()?)
}

/// Render the standard cover as RGB PNG and the X4 cover as baseline RGB JPEG.
pub fn render_cover(issue: &Issue, edition: Edition) -> Result<CoverAsset, EpubError> {
    let (width, height) = cover_size(edition);
    let svg = cover_svg(issue, edition, width, height)?;
    let pixmap = match rasterize(&svg, width, height) {
        Some(pixmap) => pixmap,
        None => {
            tracing::warn!("no usable system fonts: falling back to a geometric cover");
            draw_fallback_cover(width, height, edition)
                .ok_or_else(|| EpubError::Build("could not allocate the cover".into()))?
        }
    };
    Ok(CoverAsset {
        bytes: encode_cover(pixmap, edition)?,
        filename: cover_href(edition),
        mime: if edition == Edition::X4 {
            "image/jpeg"
        } else {
            "image/png"
        },
    })
}

fn rasterize(svg: &str, width: u32, height: u32) -> Option<tiny_skia::Pixmap> {
    let mut options = resvg::usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    if options.fontdb.is_empty() {
        return None;
    }
    let tree = resvg::usvg::Tree::from_str(svg, &options)
        .map_err(|e| tracing::warn!("cover svg did not parse: {e}"))
        .ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    pixmap.fill(tiny_skia::Color::WHITE);
    let size = tree.size();
    let transform = tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

/// Text-free cover used when font resolution fails — the build never fails (§3.10).
///
/// The badge cannot carry its lettering here, but the slab itself still keeps
/// the editions apart in a thumbnail grid.
fn draw_fallback_cover(width: u32, height: u32, edition: Edition) -> Option<tiny_skia::Pixmap> {
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    pixmap.fill(tiny_skia::Color::WHITE);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(tiny_skia::Color::BLACK);
    paint.anti_alias = true;

    let w = width as f32;
    let h = height as f32;
    let margin = w / 15.0;
    let rect = |x: f32, y: f32, rw: f32, rh: f32, pixmap: &mut tiny_skia::Pixmap| {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, y, rw, rh) {
            let path = tiny_skia::PathBuilder::from_rect(r);
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    };
    // Frame.
    let border = (w / 300.0).max(2.0);
    rect(margin, margin, w - 2.0 * margin, border, &mut pixmap);
    rect(
        margin,
        h - margin - border,
        w - 2.0 * margin,
        border,
        &mut pixmap,
    );
    rect(margin, margin, border, h - 2.0 * margin, &mut pixmap);
    rect(
        w - margin - border,
        margin,
        border,
        h - 2.0 * margin,
        &mut pixmap,
    );
    // Masthead slab plus body rules — a newspaper silhouette.
    rect(
        margin * 2.0,
        h * 0.26,
        w - 4.0 * margin,
        h * 0.035,
        &mut pixmap,
    );
    for i in 0..8 {
        let y = h * 0.42 + (i as f32) * h * 0.045;
        let inset = if i % 3 == 2 {
            margin * 4.0
        } else {
            margin * 2.0
        };
        rect(inset, y, w - 2.0 * inset, border, &mut pixmap);
    }
    if !cover_badge(edition).is_empty() {
        let badge_height = h / 16.0;
        let badge_width = w * 2.0 / 5.0;
        rect(
            (w - badge_width) / 2.0,
            h * 0.73,
            badge_width,
            badge_height,
            &mut pixmap,
        );
    }
    Some(pixmap)
}

fn encode_cover(pixmap: tiny_skia::Pixmap, edition: Edition) -> Result<Vec<u8>, EpubError> {
    let (w, h) = (pixmap.width(), pixmap.height());
    let rgba = image::RgbaImage::from_raw(w, h, pixmap.take_demultiplied())
        .ok_or_else(|| EpubError::Build("cover pixel buffer had the wrong size".into()))?;
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut bytes = Vec::new();
    match edition {
        Edition::Standard => image::DynamicImage::ImageRgb8(rgb).write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        ),
        Edition::X4 => image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 92)
            .encode_image(&image::DynamicImage::ImageRgb8(rgb)),
    }
    .map_err(|e| EpubError::Build(format!("cover encoding failed: {e}")))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Chapters (§3.10)
// ---------------------------------------------------------------------------

/// Prepare article markup for XHTML: rewrite images, sanitize, self-close voids.
fn prepare_body(html: &str, images_: &[ImageAsset]) -> String {
    // Sanitize first: image rewriting emits our own trusted markup (including the
    // `image-placeholder` class, which ammonia would otherwise strip).
    let cleaned = ammonia::clean(html);
    let rewritten = images::rewrite_img_srcs(&cleaned, images_);
    images::to_xhtml(&rewritten)
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
            images::text_escape(&issue.meta.stats_line())
        )
    } else {
        images::to_xhtml(&ammonia::clean(body))
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
            up_url: rating_url(public_url, secret, issue.meta.date, article.id, Vote::Up),
            down_url: rating_url(public_url, secret, issue.meta.date, article.id, Vote::Down),
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

fn render_cover_page(issue: &Issue, edition: Edition) -> Result<Chapter, EpubError> {
    let tpl = CoverPage {
        title: issue.meta.title_for(edition),
        alt: format!(
            "The Daily EPUB, {} \u{2014} No. {}",
            issue.meta.display_date, issue.meta.issue_number
        ),
        cover_href: cover_href(edition),
    };
    Ok(Chapter {
        id: "cover".into(),
        href: "cover.xhtml".into(),
        title: "Cover".into(),
        xhtml: tpl.render()?,
        toc_level: 1,
    })
}

/// Render every chapter for an edition, in issue order (§3.10).
pub fn render_all(
    issue: &Issue,
    edition: Edition,
    images_: &[ImageAsset],
    public_url: &str,
    hmac_secret: Option<&str>,
) -> Result<Vec<Chapter>, EpubError> {
    let mut chapters = vec![
        render_cover_page(issue, edition)?,
        render_front_page(issue)?,
        render_in_this_issue(issue)?,
    ];

    for name in section_names(issue) {
        let intro = issue
            .editorial
            .section_intros
            .get(&name)
            .map(|s| s.as_str());
        chapters.push(render_section_page(&name, intro)?);
        for pick in issue.lineup.section_picks(&name) {
            chapters.push(render_article(
                issue,
                pick,
                images_,
                edition,
                public_url,
                hmac_secret,
            )?);
            if let Some(discussion) = render_discussion(pick)? {
                chapters.push(discussion);
            }
        }
    }

    if let Some(world) = render_world_briefing(issue)? {
        chapters.push(world);
    }
    chapters.push(render_colophon(issue)?);

    if edition == Edition::X4 {
        for chapter in &mut chapters {
            chapter.xhtml = x4::simplify_xhtml(&chapter.xhtml);
        }
    }
    tracing::debug!(count = chapters.len(), ?edition, "rendered chapters");
    Ok(chapters)
}

// ---------------------------------------------------------------------------
// Assembly (§3.10)
// ---------------------------------------------------------------------------

/// A `<meta property="…">…</meta>` element for `content.opf` (§3.10).
fn opf_meta(
    property: &str,
    content: &str,
    id: Option<&str>,
    refines: Option<&str>,
) -> MetadataOpfV3 {
    MetadataOpfV3 {
        property: property.to_string(),
        content: content.to_string(),
        dir: None,
        id: id.map(str::to_string),
        refines: refines.map(str::to_string),
        scheme: None,
        xml_lang: None,
    }
}

/// The issue date, as `dcterms:date`, `dc:date` and `dcterms:issued` (§3.10).
///
/// `epub-builder` keeps `MetadataOpfV3::content` verbatim and does not export its
/// `MetadataRenderer` trait, and `set_publication_date` wants a `chrono::DateTime`
/// (not one of our dependencies), so the `<dc:date>` element rides along inside a
/// trusted, fully-controlled fragment. Deliberate for v1 and listed under
/// "known limitations" in the README; drop it if `epub-builder` ever exposes
/// `dc:date` directly (or if `chrono` joins the dependency list).
fn date_metadata(date: Date) -> MetadataOpfV3 {
    opf_meta(
        "dcterms:date",
        &format!(
            "{date}</meta>\n    <dc:date>{date}</dc:date>\n    \
             <dc:language>{LANGUAGE}</dc:language>\n    \
             <meta property=\"dcterms:issued\">{date}"
        ),
        None,
        None,
    )
}

fn epub_err(what: &str, e: impl std::fmt::Display) -> EpubError {
    EpubError::Build(format!("{what}: {e}"))
}

/// The stylesheet for an edition (§3.10 CSS).
pub fn stylesheet(edition: Edition) -> &'static str {
    match edition {
        Edition::Standard => include_str!("templates/style.css"),
        Edition::X4 => include_str!("templates/style-x4.css"),
    }
}

fn reference_type(chapter: &Chapter) -> Option<ReferenceType> {
    match chapter.id.as_str() {
        "cover" => Some(ReferenceType::Cover),
        "front" => Some(ReferenceType::Text),
        "in-this-issue" => Some(ReferenceType::Toc),
        "colophon" => Some(ReferenceType::Colophon),
        _ => None,
    }
}

/// Zip one edition into EPUB3 bytes: metadata, cover, resources, chapters, TOC.
pub fn assemble(
    issue: &Issue,
    edition: Edition,
    chapters: &[Chapter],
    images_: &[ImageAsset],
    cover: &CoverAsset,
) -> Result<Vec<u8>, EpubError> {
    let zip = ZipLibrary::new().map_err(|e| epub_err("zip library", e))?;
    let mut builder = EpubBuilder::new(zip).map_err(|e| epub_err("epub builder", e))?;
    builder.epub_version(EpubVersion::V30);
    builder
        .metadata("title", issue.meta.title_for(edition))
        .map_err(|e| epub_err("title metadata", e))?;
    builder
        .metadata("author", CREATOR)
        .map_err(|e| epub_err("author metadata", e))?;
    builder
        .metadata(
            "generator",
            format!("daily-epub {}", env!("CARGO_PKG_VERSION")),
        )
        .map_err(|e| epub_err("generator metadata", e))?;
    builder
        .metadata("toc_name", "Contents")
        .map_err(|e| epub_err("toc_name metadata", e))?;
    builder
        .metadata(
            "description",
            format!(
                "{} \u{2014} {}",
                issue.meta.display_date,
                issue.meta.stats_line()
            ),
        )
        .map_err(|e| epub_err("description metadata", e))?;

    // dc:date and the EPUB3 series metadata BookOrbit/KOReader sort on (§3.10).
    builder.add_metadata_opf(Box::new(date_metadata(issue.meta.date)));
    builder.add_metadata_opf(Box::new(opf_meta(
        "belongs-to-collection",
        COLLECTION_NAME,
        Some(COLLECTION_ID),
        None,
    )));
    builder.add_metadata_opf(Box::new(opf_meta(
        "collection-type",
        "series",
        None,
        Some(&format!("#{COLLECTION_ID}")),
    )));
    builder.add_metadata_opf(Box::new(opf_meta(
        "group-position",
        &issue.meta.issue_number.to_string(),
        None,
        Some(&format!("#{COLLECTION_ID}")),
    )));

    builder
        .stylesheet(stylesheet(edition).as_bytes())
        .map_err(|e| epub_err("stylesheet", e))?;
    builder
        .add_cover_image(cover.filename, cover.bytes.as_slice(), cover.mime)
        .map_err(|e| epub_err("cover image", e))?;
    for asset in images_ {
        builder
            .add_resource(&asset.href, asset.data.as_slice(), asset.mime.clone())
            .map_err(|e| epub_err("image resource", e))?;
    }

    for chapter in chapters {
        let mut content = EpubContent::new(chapter.href.clone(), chapter.xhtml.as_bytes())
            .title(chapter.title.clone())
            .level(i32::from(chapter.toc_level));
        if let Some(reftype) = reference_type(chapter) {
            content = content.reftype(reftype);
        }
        builder
            .add_content(content)
            .map_err(|e| epub_err("chapter", e))?;
    }

    let mut out: Vec<u8> = Vec::new();
    builder
        .generate(&mut out)
        .map_err(|e| epub_err("generate", e))?;
    Ok(out)
}

/// A synthetic, fully offline [`Issue`] used by the unit tests here and by the
/// integration tests in `tests/` (which can only see the public API).
pub mod fixtures {
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
                "<p>Body of <em>{title}</em> with an image.</p><img src=\"https://img.example/{entry_id}.png\" alt=\"A chart\"><p>More words &amp; things.</p>"
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
            summary: Some("What it argues, and why it is worth the time.".into()),
            llm: None,
            discussion: Some(discussion(1, 1001)),
        };
        let second = Pick {
            article: article(2, 1002, "A Niche Delight & Other Tales"),
            section: "Niche Corner".into(),
            position: 0,
            is_lead: false,
            summary: None,
            llm: None,
            discussion: None,
        };
        let mut section_intros = BTreeMap::new();
        section_intros.insert("Top Stories".to_string(), "The day in brief.".to_string());
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
                section_intros,
                summaries,
            },
            world_briefing: Some(WorldBriefing {
                date: "2026-08-15".parse().expect("fixed date"),
                source_url: "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_15"
                    .into(),
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
                model: "deepseek-v4-flash".into(),
                entries_fetched: 431,
                feeds_seen: 92,
                candidates: 120,
                cost_usd: 0.0731,
                generator_version: "daily-epub 0.1.0".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::issue;
    use hmac::{Hmac, KeyInit};
    use sha2::Sha256;

    #[test]
    fn rating_token_matches_the_spec_vector() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(rating_message(date, 1234, Vote::Up), "2026-08-15/1234/up");
        // hex(hmac_sha256("test-secret", "2026-08-15/1234/up"))[..16]
        let token = rating_token("test-secret", date, 1234, Vote::Up);
        assert_eq!(token.len(), TOKEN_LEN);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

        // Independently computed reference value.
        use hmac::Mac;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(b"2026-08-15/1234/up");
        let expected: String = hex::encode(mac.finalize().into_bytes())
            .chars()
            .take(16)
            .collect();
        assert_eq!(token, expected);

        // Different vote, article and secret all change the token.
        assert_ne!(token, rating_token("test-secret", date, 1234, Vote::Down));
        assert_ne!(token, rating_token("test-secret", date, 1235, Vote::Up));
        assert_ne!(token, rating_token("other-secret", date, 1234, Vote::Up));
    }

    /// The EPUB signs the links and `server.rs` verifies them: one formula, or no
    /// rating ever lands. This is the vector `server::tests` pins from its side
    /// (`VECTOR_SECRET` / `VECTOR_TOKEN_UP`) — change one, change both (§3.9).
    #[test]
    fn epub_and_server_share_one_token_vector() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(
            rating_token("test-secret", date, 42, Vote::Up),
            "3b314cf7e6d8f50f"
        );
    }

    #[test]
    fn rating_url_has_the_spec_shape() {
        let date: Date = "2026-08-15".parse().unwrap();
        let url = rating_url("https://daily.hallada.net/", "s3cret", date, 99, Vote::Down);
        let token = rating_token("s3cret", date, 99, Vote::Down);
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

    fn assert_xml_ok(xhtml: &str) {
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
        assert!(chapter.xhtml.contains("/r/2026-08-15/1/up?t="));
        assert!(chapter.xhtml.contains("/r/2026-08-15/1/down?t="));
        assert!(chapter.xhtml.contains("Read online"));
        assert!(chapter.xhtml.contains("href=\"disc-1001.xhtml\""));
        // The un-downloaded image degrades to a placeholder.
        assert!(chapter.xhtml.contains("[image: A chart]"));
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

    #[test]
    fn render_all_orders_the_issue() {
        let issue = issue();
        let chapters = render_all(&issue, Edition::Standard, &[], "https://x.test", None).unwrap();
        let ids: Vec<&str> = chapters.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "cover",
                "front",
                "in-this-issue",
                "sec-Top Stories",
                "art-1001",
                "disc-1001",
                "sec-Niche Corner",
                "art-1002",
                "world",
                "colophon",
            ]
        );
        let levels: Vec<u8> = chapters.iter().map(|c| c.toc_level).collect();
        assert_eq!(levels, vec![1, 1, 1, 1, 2, 3, 1, 2, 1, 1]);
        for chapter in &chapters {
            assert_xml_ok(&chapter.xhtml);
        }
    }

    #[test]
    fn covers_rasterize_for_both_editions() {
        let issue = issue();
        for edition in [Edition::Standard, Edition::X4] {
            let cover = render_cover(&issue, edition).expect("cover");
            let decoded = image::load_from_memory(&cover.bytes).expect("cover is a valid image");
            assert_eq!(
                (decoded.width(), decoded.height()),
                cover_size(edition),
                "cover size for {edition:?}"
            );
            assert_eq!(decoded.color(), image::ColorType::Rgb8);
            if edition == Edition::X4 {
                assert_eq!(cover.filename, "cover.jpg");
                assert_eq!(cover.mime, "image/jpeg");
                assert!(
                    cover.bytes.windows(2).any(|marker| marker == [0xff, 0xc0]),
                    "baseline SOF0 missing"
                );
                assert!(
                    !cover.bytes.windows(2).any(|marker| marker == [0xff, 0xc2]),
                    "progressive SOF2 present"
                );
            } else {
                assert_eq!(cover.filename, "cover.png");
                assert_eq!(cover.mime, "image/png");
            }
        }
    }

    #[test]
    fn fallback_cover_is_drawn_without_fonts() {
        let png = encode_cover(
            draw_fallback_cover(480, 800, Edition::X4).unwrap(),
            Edition::X4,
        )
        .unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (480, 800));
        // Some ink actually landed on the page.
        let gray = decoded.to_luma8();
        assert!(gray.pixels().any(|p| p[0] < 32));
        assert!(gray.pixels().any(|p| p[0] > 224));

        // The badge slab is the only per-edition mark the text-free fallback can
        // draw, so the two editions still differ without any fonts installed.
        let standard = draw_fallback_cover(480, 800, Edition::Standard).unwrap();
        let x4 = draw_fallback_cover(480, 800, Edition::X4).unwrap();
        assert_ne!(standard.data(), x4.data());
        assert!(badge_ink(&x4) > badge_ink(&standard));
    }

    /// Dark pixels inside the badge rectangle.
    fn badge_ink(pixmap: &tiny_skia::Pixmap) -> usize {
        let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
        let (x0, x1) = (w * 3 / 10, w * 7 / 10);
        let (y0, y1) = (h * 74 / 100, h * 78 / 100);
        let mut dark = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if pixmap.data()[(y * w + x) * 4] < 32 {
                    dark += 1;
                }
            }
        }
        dark
    }

    /// At thumbnail size the title is unreadable, so the cover itself has to
    /// say which edition it is (§3.10).
    #[test]
    fn only_the_x4_cover_carries_the_edition_badge() {
        let issue = fixtures::issue();
        let x4 = cover_svg(&issue, Edition::X4, 480, 800).unwrap();
        assert!(x4.contains(">X4 EDITION<"), "{x4}");
        // White appears twice: the page ground, and the badge text reversed out
        // of the slab.
        assert_eq!(x4.matches("fill=\"#ffffff\"").count(), 2, "{x4}");

        let standard = cover_svg(&issue, Edition::Standard, 1200, 1600).unwrap();
        assert!(!standard.contains("X4"));
        assert_eq!(standard.matches("fill=\"#ffffff\"").count(), 1);

        // The badge sits between the stats line and the footer, inside the frame.
        let cover = render_cover(&issue, Edition::X4).unwrap();
        let gray = image::load_from_memory(&cover.bytes).unwrap().to_luma8();
        let dark_in_badge = (584..634)
            .flat_map(|y| (144..336).map(move |x| (x, y)))
            .filter(|&(x, y)| gray.get_pixel(x, y)[0] < 32)
            .count();
        assert!(
            dark_in_badge > 4000,
            "badge slab is missing: {dark_in_badge}"
        );
    }
}
