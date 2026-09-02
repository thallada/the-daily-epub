//! Chapter ordering and `epub-builder` assembly (spec §3.10).
//!
//! Structure: cover → From the Editor → In This Issue → sections (title page,
//! article chapters, discussion chapters) → World Briefing → colophon. The
//! chapters themselves are rendered by [`super::chapters`] and the cover by
//! [`super::cover`]; this module decides the order and zips the result.

use epub_builder::{
    EpubBuilder, EpubContent, EpubVersion, MetadataOpfV3, ReferenceType, ZipLibrary,
};
use jiff::civil::Date;

use crate::types::{Edition, ImageAsset, Issue};

use super::EpubError;
use super::chapters::{
    render_colophon, render_front_page, render_in_this_issue, render_section_page,
    render_world_briefing, section_names,
};
use super::cover::{CoverAsset, render_cover_page};
use super::x4;

// Re-exported so callers keep one import path for "everything about building an
// issue"; the definitions live in the modules that own them.
pub use super::chapters::{
    TOKEN_LEN, prepare_body, rating_message, rating_token, rating_url, render_article,
    render_discussion, social_line,
};
pub use super::cover::{cover_badge, cover_size, render_cover};
pub use super::fixtures;

/// EPUB3 `belongs-to-collection` name (§3.10).
pub const COLLECTION_NAME: &str = "The Daily EPUB";
/// `id` the collection refinements point at (§3.10).
pub const COLLECTION_ID: &str = "daily-epub-collection";
/// `dc:creator` (§3.10).
pub const CREATOR: &str = "The Daily EPUB";
/// `dc:language` (§3.10).
pub const LANGUAGE: &str = "en";

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

/// Render every chapter of one edition, in issue order (§3.10).
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
        chapters.push(render_section_page(&name)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::fixtures::{assert_xml_ok, issue};

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
}
