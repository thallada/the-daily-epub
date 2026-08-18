//! Image download and re-encoding (spec §3.10 "Images").
//!
//! Failed downloads degrade to a `[image: alt text]` placeholder paragraph — the
//! run never fails because of an image (notes §3).

use std::collections::HashMap;
use std::io::Cursor;
use std::time::Duration;

use futures::StreamExt;
use image::{DynamicImage, GenericImageView, ImageFormat};

use crate::types::{Edition, ImageAsset, Pick};

/// Per-image download timeout (§3.10).
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 10;
/// Per-image size cap (§3.10).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Concurrent downloads (§3.10).
pub const CONCURRENCY: usize = 8;
/// Whole-issue asset budget (§3.10).
pub const ISSUE_ASSET_BUDGET_BYTES: usize = 25 * 1024 * 1024;
/// Images smaller than this in either dimension are decorative — skipped (§3.10).
pub const MIN_DIMENSION_PX: u32 = 24;
/// Width/height an SVG is rasterized to when it declares no intrinsic size.
const SVG_FALLBACK_SIZE: u32 = 1000;

/// HTML void elements: XHTML requires them self-closed (§3.10 "valid XHTML").
pub const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

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

/// One `<img>` found in article markup, with the caption of its `<figure>` if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgRef {
    pub src: String,
    pub alt: String,
    pub caption: Option<String>,
}

/// Collect `<img>` references (src, alt, enclosing figcaption) from article markup.
pub fn extract_img_refs(html: &str) -> Vec<ImgRef> {
    let doc = scraper::Html::parse_fragment(html);
    let Ok(img_sel) = scraper::Selector::parse("img") else {
        return Vec::new();
    };
    let cap_sel = scraper::Selector::parse("figcaption").ok();

    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for el in doc.select(&img_sel) {
        let Some(src) = el.value().attr("src") else {
            continue;
        };
        let src = src.trim();
        if src.is_empty() || src.starts_with("data:") {
            continue;
        }
        if seen.iter().any(|s| s == src) {
            continue;
        }
        seen.push(src.to_string());
        let alt = el
            .value()
            .attr("alt")
            .unwrap_or_default()
            .trim()
            .to_string();
        // Walk up to an enclosing <figure> and take its caption, if any.
        let mut caption = None;
        if let Some(cap_sel) = &cap_sel {
            let mut cursor = el.parent();
            while let Some(node) = cursor {
                if let Some(elem) = scraper::ElementRef::wrap(node) {
                    if elem.value().name() == "figure" {
                        caption = elem.select(cap_sel).next().map(|c| {
                            c.text()
                                .collect::<String>()
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                        });
                        break;
                    }
                    cursor = elem.parent();
                } else {
                    break;
                }
            }
        }
        out.push(ImgRef {
            src: src.to_string(),
            alt,
            caption: caption.filter(|c| !c.is_empty()),
        });
    }
    out
}

/// Download one image, honoring the timeout and size cap (§3.10).
pub async fn download(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = http
        .get(url)
        // Some CDNs answer `Accept: */*` with an HTML interstitial (§3.10).
        .header(reqwest::header::ACCEPT, "image/*,*/*;q=0.8")
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| tracing::debug!(url, "image download failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(url, status = %resp.status(), "image download rejected");
        return None;
    }
    if let Some(len) = resp.content_length()
        && len as usize > MAX_IMAGE_BYTES
    {
        tracing::debug!(url, len, "image exceeds the size cap");
        return None;
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
                    tracing::debug!(url, "image exceeds the size cap mid-stream");
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(url, "image download interrupted: {e}");
                return None;
            }
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
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

/// Everything needed to fetch one image, in deterministic issue order.
#[derive(Debug, Clone)]
struct PendingImage {
    id: String,
    url: String,
    alt: String,
    caption: Option<String>,
}

fn pending_for_pick(pick: &Pick) -> Vec<PendingImage> {
    let entry_id = pick.article.best_entry_id;
    let mut refs = extract_img_refs(&pick.article.content_html);
    if refs.is_empty() {
        refs = pick
            .article
            .image_urls
            .iter()
            .map(|u| ImgRef {
                src: u.clone(),
                alt: String::new(),
                caption: None,
            })
            .collect();
    }
    refs.into_iter()
        .filter(|r| r.src.starts_with("http://") || r.src.starts_with("https://"))
        .enumerate()
        .map(|(i, r)| PendingImage {
            id: format!("img-{entry_id}-{i}"),
            url: r.src,
            alt: r.alt,
            caption: r.caption,
        })
        .collect()
}

/// Download and re-encode every image referenced by the lineup for one edition,
/// respecting [`ISSUE_ASSET_BUDGET_BYTES`] (§3.10).
pub async fn collect_for_issue(
    http: &reqwest::Client,
    picks: &[Pick],
    edition: Edition,
) -> Vec<ImageAsset> {
    let profile = ImageProfile::for_edition(edition);
    let pending: Vec<PendingImage> = picks.iter().flat_map(pending_for_pick).collect();
    if pending.is_empty() {
        return Vec::new();
    }
    tracing::info!(count = pending.len(), ?edition, "downloading issue images");

    let results: Vec<Option<(PendingImage, Vec<u8>, &'static str)>> =
        futures::stream::iter(pending.into_iter().map(|p| {
            let http = http.clone();
            async move {
                let raw = download(&http, &p.url).await?;
                let (bytes, mime) = tokio::task::spawn_blocking(move || reencode(&raw, profile))
                    .await
                    .ok()
                    .flatten()?;
                Some((p, bytes, mime))
            }
        }))
        .buffered(CONCURRENCY)
        .collect()
        .await;

    let mut assets = Vec::new();
    let mut budget_used = 0usize;
    let mut skipped = 0usize;
    for result in results.into_iter().flatten() {
        let (pending, bytes, mime) = result;
        if budget_used + bytes.len() > ISSUE_ASSET_BUDGET_BYTES {
            skipped += 1;
            continue;
        }
        budget_used += bytes.len();
        let ext = if mime == "image/png" { "png" } else { "jpg" };
        assets.push(ImageAsset {
            href: format!("images/{}.{ext}", pending.id),
            id: pending.id,
            mime: mime.to_string(),
            data: bytes,
            alt: pending.alt,
            caption: pending.caption,
            source_url: pending.url,
        });
    }
    if skipped > 0 {
        tracing::warn!(skipped, budget_used, "issue image budget exhausted");
    }
    tracing::info!(
        embedded = assets.len(),
        bytes = budget_used,
        "issue images ready"
    );
    assets
}

// ---------------------------------------------------------------------------
// Markup rewriting
// ---------------------------------------------------------------------------

/// End index (exclusive) of the tag starting at `start` (`html[start] == '<'`),
/// respecting quoted attribute values and comments.
pub(crate) fn tag_end(html: &str, start: usize) -> Option<usize> {
    let rest = &html[start..];
    if rest.starts_with("<!--") {
        return rest.find("-->").map(|i| start + i + 3);
    }
    let mut quote: Option<char> = None;
    for (i, c) in rest.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '>') => return Some(start + i + c.len_utf8()),
            (None, _) => {}
        }
    }
    None
}

/// Lowercased element name of a tag body such as `img src="…"`.
pub(crate) fn tag_name(inner: &str) -> String {
    inner
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Parse `name="value"` pairs out of a tag body, with entity-decoded values.
///
/// Decoding matters: this scanner reads markup that ammonia has serialized, and
/// ammonia writes `&` in a URL as `&amp;`. A raw comparison against a URL that
/// came out of a real HTML parser would never match (§3.10).
pub(crate) fn parse_attrs(inner: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes: Vec<char> = inner.chars().collect();
    let mut i = 0;
    // Skip the element name.
    while i < bytes.len() && !bytes[i].is_whitespace() {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_whitespace() || bytes[i] == '/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_whitespace() && bytes[i] != '=' && bytes[i] != '/' {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name: String = bytes[name_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == '=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == '"' || bytes[i] == '\'') {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    value.push(bytes[i]);
                    i += 1;
                }
                i += 1;
            } else {
                while i < bytes.len() && !bytes[i].is_whitespace() && bytes[i] != '>' {
                    value.push(bytes[i]);
                    i += 1;
                }
            }
        }
        attrs.push((name, decode_entities(&value)));
    }
    attrs
}

/// Decode the handful of entities an HTML serializer emits inside attributes.
///
/// Numeric forms are included because feeds and WordPress write `&#038;` for
/// `&`; anything else is left alone rather than guessed at.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escape a string for use inside a double-quoted XML attribute.
fn attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape a string for XML text content.
pub fn text_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether an image we could not embed is worth telling the reader about.
///
/// Only descriptive alt text qualifies. A filename, a bare label like `red line`
/// on a divider rule, or no alt at all carries nothing the reader loses by not
/// seeing the picture — announcing those turns every decorative graphic and
/// dead link into a line of clutter, which is how the placeholders got out of
/// hand in the first place.
fn alt_is_worth_announcing(alt: &str) -> bool {
    const MIN_DESCRIPTIVE_WORDS: usize = 4;
    !alt.is_empty()
        && !is_filename_alt(alt)
        && alt.split_whitespace().count() >= MIN_DESCRIPTIVE_WORDS
}

/// True for alt text that is really just the uploaded filename — `IMG_0808.JPG`,
/// `cut pieces v01.JPG`, `chart-final-2.png`.
fn is_filename_alt(alt: &str) -> bool {
    let alt = alt.trim();
    if alt.contains(' ') && alt.split_whitespace().count() > 4 {
        return false;
    }
    let Some((stem, ext)) = alt.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "avif" | "bmp" | "heic"
    ) && !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
}

/// Rewrite `<img src>` to the embedded hrefs, replacing misses with the
/// `[image: alt]` placeholder paragraph (§3.10).
pub fn rewrite_img_srcs(html: &str, assets: &[ImageAsset]) -> String {
    let by_url: HashMap<&str, &ImageAsset> =
        assets.iter().map(|a| (a.source_url.as_str(), a)).collect();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        let Some(end) = tag_end(html, start) else {
            out.push_str(&html[start..]);
            return out;
        };
        let raw = &html[start..end];
        let inner = raw
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim_end_matches('/');
        if tag_name(inner) == "img" {
            let attrs = parse_attrs(inner);
            let src = attrs
                .iter()
                .find(|(k, _)| k == "src")
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default();
            let alt = attrs
                .iter()
                .find(|(k, _)| k == "alt")
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default();
            match by_url.get(src.as_str()) {
                Some(asset) => {
                    let alt = if alt.is_empty() { &asset.alt } else { &alt };
                    out.push_str(&format!(
                        "<img src=\"{}\" alt=\"{}\"/>",
                        attr_escape(&asset.href),
                        attr_escape(alt)
                    ));
                }
                // An image we could not embed is only worth announcing when its
                // alt text tells the reader something; otherwise the `<img>`
                // just goes away. That covers the decorative graphics the
                // re-encoder deliberately skips as well as genuine misses (§3.10).
                None if !alt_is_worth_announcing(&alt) => {}
                None => {
                    out.push_str(&format!(
                        "<p class=\"image-placeholder\">[image: {}]</p>",
                        text_escape(&alt)
                    ));
                }
            }
        } else {
            out.push_str(raw);
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// Self-close HTML void elements and normalize `&nbsp;` so the markup parses as
/// XML — EPUB3 content documents are XHTML (§3.10).
pub fn to_xhtml(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        let Some(end) = tag_end(html, start) else {
            out.push_str(&html[start..]);
            cursor = html.len();
            break;
        };
        let raw = &html[start..end];
        let inner = raw.trim_start_matches('<').trim_end_matches('>');
        let name = tag_name(inner);
        if VOID_ELEMENTS.contains(&name.as_str()) && !inner.trim_end().ends_with('/') {
            out.push('<');
            out.push_str(inner.trim_end());
            out.push_str("/>");
        } else {
            out.push_str(raw);
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    // html5ever (via ammonia) emits `&nbsp;`, which is undefined in XML.
    out.replace("&nbsp;", "&#160;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(url: &str, href: &str) -> ImageAsset {
        ImageAsset {
            id: "img-1-0".into(),
            href: href.into(),
            mime: "image/jpeg".into(),
            data: vec![1, 2, 3],
            alt: "fallback alt".into(),
            caption: None,
            source_url: url.into(),
        }
    }

    #[test]
    fn extracts_img_refs_with_captions() {
        let html = r#"<p>hi</p>
            <figure><img src="https://e.g/a.png" alt="A diagram"/>
            <figcaption>Figure 1:  the thing</figcaption></figure>
            <img src="https://e.g/b.jpg"/>
            <img src="data:image/png;base64,zz"/>
            <img src="https://e.g/a.png" alt="dupe"/>"#;
        let refs = extract_img_refs(html);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].src, "https://e.g/a.png");
        assert_eq!(refs[0].alt, "A diagram");
        assert_eq!(refs[0].caption.as_deref(), Some("Figure 1: the thing"));
        assert_eq!(refs[1].src, "https://e.g/b.jpg");
        assert!(refs[1].caption.is_none());
    }

    #[test]
    fn rewrites_hits_and_placeholders_misses() {
        let assets = vec![asset("https://e.g/a.png", "images/img-1-0.jpg")];
        let html = r#"<p>x</p><img src="https://e.g/a.png" alt="Alt &amp; more"><img src="https://e.g/gone.png" alt="A chart of missing things">"#;
        let out = rewrite_img_srcs(html, &assets);
        // The alt round-trips through one level of escaping, not two.
        assert!(out.contains(r#"<img src="images/img-1-0.jpg" alt="Alt &amp; more"/>"#));
        assert!(
            out.contains(r#"<p class="image-placeholder">[image: A chart of missing things]</p>"#)
        );
        assert!(!out.contains("gone.png"));
    }

    /// The whole point of the fix: ammonia writes `&amp;` into the markup, and
    /// the asset was keyed on the URL a real parser produced.
    #[test]
    fn entity_encoded_urls_still_match_their_asset() {
        let assets = vec![asset(
            "https://e.g/a.jpg?id=1&width=980",
            "images/img-1-0.jpg",
        )];
        let html = r#"<img src="https://e.g/a.jpg?id=1&amp;width=980" alt="Chart"/>"#;
        assert!(rewrite_img_srcs(html, &assets).contains(r#"src="images/img-1-0.jpg""#));
        // The numeric spelling WordPress emits works too.
        let html = r#"<img src="https://e.g/a.jpg?id=1&#038;width=980" alt="Chart"/>"#;
        assert!(rewrite_img_srcs(html, &assets).contains(r#"src="images/img-1-0.jpg""#));
    }

    #[test]
    fn unembeddable_images_only_speak_up_when_the_alt_says_something() {
        // No alt at all: the image simply disappears.
        assert_eq!(
            rewrite_img_srcs(r#"<img src="https://e.g/x.png">"#, &[]),
            ""
        );
        // A filename is not a description.
        assert_eq!(
            rewrite_img_srcs(r#"<img src="https://e.g/x.png" alt="IMG_0808.JPG">"#, &[]),
            ""
        );
        // Neither is the label on a decorative divider rule.
        assert_eq!(
            rewrite_img_srcs(r#"<img src="https://e.g/rule.png" alt="red line">"#, &[]),
            ""
        );
        // A real description is worth keeping.
        assert!(
            rewrite_img_srcs(
                r#"<img src="https://e.g/x.png" alt="A man in a hard hat stands over a well hole">"#,
                &[]
            )
            .contains("[image: A man in a hard hat stands over a well hole]")
        );
    }

    #[test]
    fn filename_alt_detection() {
        for yes in [
            "IMG_0808.JPG",
            "cut pieces v01.JPG",
            "chart-final-2.png",
            "diagram.svg",
        ] {
            assert!(is_filename_alt(yes), "{yes} should read as a filename");
        }
        for no in [
            "A hydrogen well head",
            "",
            "Fig. 3",
            "The lion-man of Hohlenstein-Stadel, carved from mammoth ivory.",
        ] {
            assert!(!is_filename_alt(no), "{no} should read as a description");
        }
    }

    #[test]
    fn to_xhtml_self_closes_voids_and_entities() {
        let html = "<p>a<br>b<hr>c&nbsp;d<img src=\"x.png\" alt=\"y\"></p><p>e<br/></p>";
        let out = to_xhtml(html);
        assert!(out.contains("<br/>"));
        assert!(out.contains("<hr/>"));
        assert!(out.contains("<img src=\"x.png\" alt=\"y\"/>"));
        assert!(out.contains("&#160;"));
        assert!(!out.contains("&nbsp;"));
        assert!(!out.contains("<br/ >"));
        // Already-closed voids are left alone (no double slash).
        assert_eq!(out.matches("<br/>").count(), 2);
    }

    #[test]
    fn tag_scanner_ignores_angle_brackets_in_attributes() {
        let html = r#"<a title="a > b">x</a>"#;
        assert_eq!(to_xhtml(html), html);
    }

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
