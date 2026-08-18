//! The generated cover image and cover page (spec §3.10).
//!
//! The cover is drawn as an SVG from a template, rasterized with resvg and
//! encoded per edition. If font resolution fails the run still gets a cover —
//! a text-free variant that keeps the editions apart in a library thumbnail
//! grid (notes §3: nothing here fails a build).

use askama::Template;

use crate::types::{Edition, Issue};

use super::EpubError;
use super::build::Chapter;
use super::x4;

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

pub fn render_cover_page(issue: &Issue, edition: Edition) -> Result<Chapter, EpubError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::fixtures::{self, issue};

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
