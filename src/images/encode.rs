//! Turning downloaded bytes into something an e-reader can display.
//!
//! Every image is decoded, fitted to the edition's profile, flattened onto white
//! (e-ink has no transparency) and re-encoded. SVG is rasterized on the way in,
//! because charts and diagrams are frequently vector-only.

use std::io::Cursor;

use image::{DynamicImage, GenericImageView, ImageFormat};

use crate::types::Edition;

/// Images smaller than this in either dimension are decorative — skipped (§3.10).
pub const MIN_DIMENSION_PX: u32 = 24;
/// Width an SVG is rendered at when the profile asks for less than this.
const SVG_FALLBACK_SIZE: u32 = 1000;

/// Per-edition re-encoding parameters (§3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageProfile {
    pub max_width: u32,
    pub max_height: u32,
    pub jpeg_quality: u8,
    pub grayscale: bool,
}

impl ImageProfile {
    /// Standard edition: max width 1200px, JPEG q80, color (§3.10).
    pub const STANDARD: ImageProfile = ImageProfile {
        max_width: 1200,
        max_height: 4000,
        jpeg_quality: 80,
        grayscale: false,
    };

    /// X4 edition: grayscale Luma8, fit within 480×800, JPEG q70 (§3.10).
    pub const X4: ImageProfile = ImageProfile {
        max_width: 480,
        max_height: 800,
        jpeg_quality: 70,
        grayscale: true,
    };

    pub fn for_edition(edition: Edition) -> Self {
        match edition {
            Edition::Standard => Self::STANDARD,
            Edition::X4 => Self::X4,
        }
    }
}

/// Decode, resize/grayscale, flatten transparency to white and re-encode (§3.10).
///
/// Line art with transparency is kept as PNG after flattening; everything else
/// becomes JPEG. SVG is rasterized first — charts and diagrams are frequently
/// vector-only, and dropping them loses the point of the article. Returns `None`
/// for sources no decoder handles.
pub fn reencode(bytes: &[u8], profile: ImageProfile) -> Option<(Vec<u8>, &'static str)> {
    if looks_like_svg(bytes) {
        let raster = rasterize_svg(bytes, profile)?;
        return reencode(&raster, profile);
    }
    let format = image::guess_format(bytes).ok();
    let decoded = image::load_from_memory(bytes)
        .map_err(|e| tracing::debug!("undecodable image: {e}"))
        .ok()?;

    let (w, h) = decoded.dimensions();
    if w < MIN_DIMENSION_PX || h < MIN_DIMENSION_PX {
        tracing::debug!(w, h, "skipping decorative image");
        return None;
    }

    let has_alpha = decoded.color().has_alpha();
    let flattened = if has_alpha {
        flatten_to_white(&decoded)
    } else {
        decoded
    };

    let resized = if w > profile.max_width || h > profile.max_height {
        flattened.resize(
            profile.max_width,
            profile.max_height,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        flattened
    };

    // Keep line art (PNG source, few distinct tones) lossless; everything else
    // becomes JPEG, which is far smaller for photographs (§3.10).
    let keep_png = format == Some(ImageFormat::Png) && is_line_art(&resized);

    let mut out = Cursor::new(Vec::new());
    // NB: encode the concrete buffer, not the `DynamicImage` — the latter always
    // reports RGBA pixels, which would silently re-colorize a grayscale image.
    if profile.grayscale {
        let gray = resized.to_luma8();
        if keep_png {
            DynamicImage::ImageLuma8(gray)
                .write_to(&mut out, ImageFormat::Png)
                .ok()?;
            return Some((out.into_inner(), "image/png"));
        }
        let mut enc =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, profile.jpeg_quality);
        enc.encode_image(&gray).ok()?;
        return Some((out.into_inner(), "image/jpeg"));
    }

    let rgb = resized.to_rgb8();
    if keep_png {
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut out, ImageFormat::Png)
            .ok()?;
        return Some((out.into_inner(), "image/png"));
    }
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, profile.jpeg_quality);
    enc.encode_image(&rgb).ok()?;
    Some((out.into_inner(), "image/jpeg"))
}

/// True when `bytes` are an SVG document (possibly behind an XML prolog or BOM).
fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with("<svg")
        || (text.starts_with("<?xml") || text.starts_with("<!DOCTYPE svg")) && text.contains("<svg")
}

/// Rasterize an SVG to a PNG at the profile's target width (§3.10).
///
/// The profile's own resize pass then handles the height cap, so this only has
/// to land in the right ballpark.
fn rasterize_svg(bytes: &[u8], profile: ImageProfile) -> Option<Vec<u8>> {
    let mut options = resvg::usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(bytes, &options)
        .map_err(|e| tracing::debug!("svg did not parse: {e}"))
        .ok()?;

    // An `<svg>` that parses but draws nothing is not an image, it is a stray
    // tag: rasterizing it would embed a blank rectangle.
    if tree.root().children().is_empty() {
        tracing::debug!("svg has nothing to draw");
        return None;
    }
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if !(sw.is_finite() && sh.is_finite()) || sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    // Judge "decorative" by the declared size, before scaling: a 16×16 icon is
    // an icon however large we choose to draw it.
    if sw < MIN_DIMENSION_PX as f32 || sh < MIN_DIMENSION_PX as f32 {
        tracing::debug!(sw, sh, "skipping decorative svg");
        return None;
    }
    // Vector art has no native resolution, so render straight at the edition's
    // target width — upscaling a rasterized copy afterwards would only blur it.
    let target_w = profile
        .max_width
        .max(SVG_FALLBACK_SIZE.min(profile.max_width));
    let scale = (target_w as f32 / sw).min(profile.max_height as f32 / sh);
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let (w, h) = (
        (sw * scale).round().max(1.0) as u32,
        (sh * scale).round().max(1.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    // E-ink has no transparency; render onto white so alpha never becomes black.
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba = image::RgbaImage::from_raw(w, h, pixmap.take_demultiplied())?;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(rgba)
        .write_to(&mut png, ImageFormat::Png)
        .ok()?;
    Some(png.into_inner())
}

/// Composite over an opaque white page — e-ink has no transparency (§3.10).
fn flatten_to_white(img: &DynamicImage) -> DynamicImage {
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = f32::from(px[3]) / 255.0;
        let blend = |c: u8| {
            ((f32::from(c) * a) + 255.0 * (1.0 - a))
                .round()
                .clamp(0.0, 255.0) as u8
        };
        rgb.put_pixel(x, y, image::Rgb([blend(px[0]), blend(px[1]), blend(px[2])]));
    }
    DynamicImage::ImageRgb8(rgb)
}

/// Cheap line-art test: few distinct colors (diagrams, logos, screenshots of text).
fn is_line_art(img: &DynamicImage) -> bool {
    const SAMPLE_LIMIT: usize = 20_000;
    const DISTINCT_LIMIT: usize = 64;
    let rgb = img.to_rgb8();
    let mut distinct: Vec<[u8; 3]> = Vec::with_capacity(DISTINCT_LIMIT + 1);
    for (i, px) in rgb.pixels().enumerate() {
        if i >= SAMPLE_LIMIT {
            break;
        }
        let c = [px[0], px[1], px[2]];
        if !distinct.contains(&c) {
            distinct.push(c);
            if distinct.len() > DISTINCT_LIMIT {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reencode_resizes_grayscales_and_encodes() {
        let mut img = image::RgbaImage::new(200, 100);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let raw = png.into_inner();

        let (std_bytes, std_mime) = reencode(&raw, ImageProfile::STANDARD).unwrap();
        assert_eq!(std_mime, "image/jpeg");
        let decoded = image::load_from_memory(&std_bytes).unwrap();
        assert_eq!(decoded.dimensions(), (200, 100), "no upscaling");

        let (x4_bytes, _) = reencode(&raw, ImageProfile::X4).unwrap();
        let x4 = image::load_from_memory(&x4_bytes).unwrap();
        assert!(x4.width() <= 480 && x4.height() <= 800);
        assert_eq!(x4.color(), image::ColorType::L8, "X4 is grayscale Luma8");
    }

    #[test]
    fn reencode_skips_decorative_images_and_junk() {
        let tiny = image::RgbaImage::new(8, 8);
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(tiny)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        assert!(reencode(&png.into_inner(), ImageProfile::STANDARD).is_none());
        // A bare `<svg>` tag with nothing to draw is markup, not a picture.
        assert!(reencode(b"<svg>not an image</svg>", ImageProfile::STANDARD).is_none());
        assert!(reencode(b"not an image at all", ImageProfile::STANDARD).is_none());
    }

    #[test]
    fn svg_charts_are_rasterized_rather_than_dropped() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300">
            <rect x="10" y="10" width="380" height="280" fill="#3355bb"/>
            <circle cx="200" cy="150" r="60" fill="#ffcc00"/>
        </svg>"##;
        let (bytes, mime) = reencode(svg, ImageProfile::STANDARD).expect("svg rasterizes");
        assert_eq!(mime, "image/png", "flat colour art stays lossless");
        let decoded = image::load_from_memory(&bytes).expect("decodable output");
        // Drawn at the edition's target width, not at the SVG's nominal size.
        assert_eq!(decoded.dimensions(), (1200, 900));
        assert!(!decoded.color().has_alpha(), "rendered onto white");

        // An XML prolog and a leading BOM must not hide the format.
        let with_prolog = format!(
            "\u{feff}<?xml version=\"1.0\"?>{}",
            String::from_utf8_lossy(svg)
        );
        let (x4, _) = reencode(with_prolog.as_bytes(), ImageProfile::X4).expect("x4 rasterizes");
        let x4 = image::load_from_memory(&x4).unwrap();
        assert!(x4.width() <= 480 && x4.height() <= 800);

        // A 16×16 icon is decorative however large we could draw it.
        let icon = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
            <rect width="16" height="16"/></svg>"#;
        assert!(reencode(icon, ImageProfile::STANDARD).is_none());
    }

    #[test]
    fn line_art_png_stays_png_and_is_flattened() {
        let mut img = image::RgbaImage::new(120, 60);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x % 12 == 0 {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([255, 255, 255, 0])
            };
        }
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let (bytes, mime) = reencode(&png.into_inner(), ImageProfile::STANDARD).unwrap();
        assert_eq!(mime, "image/png");
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(!decoded.color().has_alpha(), "transparency is flattened");
        // Transparent pixels became white.
        assert_eq!(
            decoded.to_rgb8().get_pixel(1, 1),
            &image::Rgb([255, 255, 255])
        );
    }
}
