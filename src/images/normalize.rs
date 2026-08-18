//! Making a page's `<img>` elements usable before anything else touches them.
//!
//! Publishers ship images in a dozen incompatible shapes: lazy placeholders,
//! `srcset` lists, `<picture>` sources, URLs parked in `data-*` attributes, and
//! `src` values that are not URLs at all (unfilled templates, JSON blobs, whole
//! `srcset` strings). On top of that, readability actively damages images while
//! it works — it deletes `<button>` subtrees, taking lightbox images with them,
//! and its own lazy-image heuristic overwrites a working `src` with any
//! attribute whose value happens to contain `.jpg`.
//!
//! [`prepare_for_readability`] runs before readability and leaves every `<img>`
//! as a plain `src`/`alt`/`title` triple, which both fixes the input and denies
//! readability the raw material for its substitution. [`normalize_img_tags`] is
//! also applied to feed content, which carries the same markup.
//!
//! Candidates are judged by *shape*, never by publisher: a string with braces,
//! whitespace or quotes in it cannot resolve, whoever wrote it.

use crate::html::{html_to_text, parse_attrs, tag_end, tag_name, truncate_utf8};

/// Elements that readability deletes outright, and which a page may nevertheless
/// have wrapped around an image (lightbox triggers, mostly).
const IMAGE_WRAPPER_TAGS: &[&str] = &["button", "form", "fieldset", "object"];

/// Attributes lazy-loading libraries use for the real image URL.
///
/// These outrank `src`, because a page only sets them when `src` is a stand-in:
/// a transparent GIF, a blurred thumbnail, an inline SVG spacer. Attributes that
/// merely *look* image-ish (`data-template`, `data-attrs`, `data-orig-file`) are
/// deliberately absent — those hold templates and metadata, and preferring them
/// is exactly the mistake readability's own heuristic makes.
const LAZY_SRC_ATTRS: &[&str] = &[
    "data-src",
    "data-lazy-src",
    "data-original",
    "data-runner-src",
    "data-full-src",
    "data-hi-res-src",
    "data-image-src",
];

/// Widest `srcset` candidate we will pick; above this we are downloading pixels
/// the re-encoder immediately throws away.
const MAX_SRCSET_WIDTH: u32 = 2000;

/// Make a fetched page safe to hand to readability (§3.3).
///
/// Two passes, both about images: unwrap the elements that would take an image
/// with them when readability deletes them, then reduce every `<img>` to a plain
/// `src`/`alt`/`title` triple. The second pass is what stops readability's own
/// lazy-image heuristic from replacing a working `src` — with no `srcset`,
/// `loading` or `data-*` attributes left on the element, it has nothing to
/// substitute and leaves the image alone.
pub fn prepare_for_readability(html: &str) -> String {
    normalize_img_tags(&unwrap_image_wrappers(html))
}

/// Replace image-only `<button>`/`<form>`/`<fieldset>`/`<object>` wrappers with
/// their contents (§3.3).
///
/// A `<button>` holding nothing but an image is a lightbox trigger, not a
/// control: the image is the content. Wrappers that also carry text are left
/// alone, because those really are interface.
pub fn unwrap_image_wrappers(html: &str) -> String {
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
        let inner = raw.trim_start_matches('<').trim_end_matches('>');
        let name = tag_name(inner);

        if IMAGE_WRAPPER_TAGS.contains(&name.as_str())
            && !inner.trim_end().ends_with('/')
            && let Some((content, after)) = element_content(html, end, &name)
            && content.contains("<img")
            && html_to_text(content).trim().is_empty()
        {
            out.push_str(&unwrap_image_wrappers(content));
            cursor = after;
            continue;
        }

        out.push_str(raw);
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// The content of an element whose open tag ended at `body_start`, plus the
/// offset just past its close tag. `None` when the element is never closed.
fn element_content<'a>(html: &'a str, body_start: usize, name: &str) -> Option<(&'a str, usize)> {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut depth = 1usize;
    let mut cursor = body_start;
    while let Some(rel) = html[cursor..].find('<') {
        let start = cursor + rel;
        let end = tag_end(html, start)?;
        let tag = &html[start..end];
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some((&html[body_start..start], end));
            }
        } else if lower.starts_with(&open)
            && !lower[open.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '-')
            && !tag.trim_end().ends_with("/>")
        {
            depth += 1;
        }
        cursor = end;
    }
    None
}

/// Reduce every `<img>` to `<img src alt title>` with a usable URL (§3.3).
///
/// The `src` a page ships is not automatically the one to use: it can be a lazy
/// placeholder (`data:image/svg+xml,…`), an unresolved template
/// (`…/resize/{width}/…`), a JSON blob a framework parked there, or an entire
/// `srcset` string. Candidates are tried in order and the first plausible one
/// wins; an image with no plausible candidate is dropped, because a broken
/// `<img>` only becomes clutter further down the pipeline.
pub fn normalize_img_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    // `<picture>` puts the real candidates on sibling `<source>` elements.
    let mut picture_srcset: Option<String> = None;

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
        let name = tag_name(inner);

        match name.as_str() {
            "picture" => {
                picture_srcset = None;
                out.push_str(raw);
            }
            "source" => {
                let attrs = parse_attrs(inner);
                if picture_srcset.is_none()
                    && let Some(set) =
                        attr(&attrs, "srcset").or_else(|| attr(&attrs, "data-srcset"))
                {
                    picture_srcset = Some(set.to_string());
                }
                out.push_str(raw);
            }
            "img" => {
                let attrs = parse_attrs(inner);
                if let Some(src) = best_img_src(&attrs, picture_srcset.as_deref()) {
                    out.push_str("<img src=\"");
                    out.push_str(&escape_attr(&src));
                    out.push('"');
                    for key in ["alt", "title"] {
                        if let Some(v) = attr(&attrs, key) {
                            out.push(' ');
                            out.push_str(key);
                            out.push_str("=\"");
                            out.push_str(&escape_attr(v));
                            out.push('"');
                        }
                    }
                    out.push_str("/>");
                } else {
                    tracing::debug!(tag = %truncate_utf8(raw, 120), "dropping unusable img");
                }
            }
            _ => out.push_str(raw),
        }
        if name == "picture" && inner.starts_with('/') {
            picture_srcset = None;
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// Pick the best URL for one `<img>` from everything the element carries.
fn best_img_src(attrs: &[(String, String)], picture_srcset: Option<&str>) -> Option<String> {
    for key in LAZY_SRC_ATTRS {
        if let Some(v) = attr(attrs, key).filter(|s| plausible_url(s)) {
            return Some(v.to_string());
        }
    }
    if let Some(src) = attr(attrs, "src").filter(|s| plausible_url(s)) {
        return Some(src.to_string());
    }
    if let Some(from_set) = attr(attrs, "srcset").and_then(best_from_srcset) {
        return Some(from_set);
    }
    if let Some(from_set) = attr(attrs, "data-srcset").and_then(best_from_srcset) {
        return Some(from_set);
    }
    picture_srcset.and_then(best_from_srcset)
}

/// The widest candidate in a `srcset` that is still worth downloading.
///
/// Parsed by whitespace rather than by comma: the URLs of several image CDNs
/// contain commas of their own, and splitting on those shreds them.
fn best_from_srcset(srcset: &str) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    let mut smallest: Option<(u32, String)> = None;
    let mut pending: Option<String> = None;

    let mut consider = |url: String, width: u32| {
        if width <= MAX_SRCSET_WIDTH && best.as_ref().is_none_or(|(w, _)| width > *w) {
            best = Some((width, url.clone()));
        }
        if smallest.as_ref().is_none_or(|(w, _)| width < *w) {
            smallest = Some((width, url));
        }
    };

    for token in srcset.split_whitespace() {
        let token = token.trim_end_matches(',');
        if token.is_empty() {
            continue;
        }
        match parse_descriptor(token) {
            Some(width) => {
                if let Some(url) = pending.take() {
                    consider(url, width);
                }
            }
            None => {
                // A URL with no descriptor of its own still counts, at width 1x.
                if let Some(url) = pending.replace(token.to_string()) {
                    consider(url, 1);
                }
            }
        }
    }
    if let Some(url) = pending.take() {
        consider(url, 1);
    }

    best.or(smallest)
        .map(|(_, url)| url)
        .filter(|u| plausible_url(u))
}

/// `800w` → 800, `2x` → a synthetic width so density candidates sort sensibly.
fn parse_descriptor(token: &str) -> Option<u32> {
    let (value, unit) = token.split_at(token.len().checked_sub(1)?);
    match unit {
        "w" => value.parse::<u32>().ok(),
        "x" => value
            .parse::<f32>()
            .ok()
            .map(|d| (d * 1000.0).round().clamp(1.0, f32::from(u16::MAX)) as u32),
        _ => None,
    }
}

/// Whether a string can serve as an image URL at all.
///
/// This is deliberately about shape, not about the host: whitespace, braces and
/// quotes mean we are looking at a `srcset` blob, an unfilled URL template or a
/// serialized object, none of which will ever resolve.
fn plausible_url(candidate: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.len() > 2048 {
        return false;
    }
    if candidate.starts_with("data:") || candidate.starts_with("about:") {
        return false;
    }
    if candidate
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '{' | '}' | '"' | '\'' | '<' | '>' | '\\'))
    {
        return false;
    }
    // `%20` is a space that survived encoding — same blob, different spelling.
    !candidate.contains("%20")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};

    /// The `src` values that survive normalization, read back with a real parser.
    fn src_of(html: &str) -> Vec<String> {
        Html::parse_fragment(html)
            .select(&Selector::parse("img").unwrap())
            .filter_map(|e| e.value().attr("src").map(str::to_string))
            .collect()
    }

    #[test]
    fn lazy_placeholder_src_gives_way_to_the_real_url() {
        // IEEE Spectrum: an inline SVG spacer with the URL parked on data-runner-src.
        let html = r#"<img alt="A well head" lazy-loadable="true"
            src="data:image/svg+xml,%3Csvg%20xmlns=%27http://www.w3.org/2000/svg%27%3E%3C/svg%3E"
            data-runner-src="https://spectrum.ieee.org/media-library/well.jpg?id=675&amp;width=980"/>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://spectrum.ieee.org/media-library/well.jpg?id=675&width=980"]
        );
    }

    #[test]
    fn unresolved_url_templates_fall_through_to_a_real_candidate() {
        // NPR: readability copies data-template over a perfectly good src.
        let html = r#"<img alt="Meghan Cliffel"
            src="https://npr.brightspotcdn.com/resize/{width}/quality/{quality}/x.jpg"
            srcset="https://npr.brightspotcdn.com/resize/1100/quality/50/x.jpg 1100w"/>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://npr.brightspotcdn.com/resize/1100/quality/50/x.jpg"]
        );
        // With nothing usable anywhere, the image goes rather than becoming a
        // request for a picture that says "Image".
        let only_template =
            r#"<img alt="x" src="https://cdn.dev/resize/{width}/quality/{quality}/x.jpg"/>"#;
        assert!(src_of(&normalize_img_tags(only_template)).is_empty());
    }

    #[test]
    fn a_srcset_blob_parked_in_src_is_rejected_and_reparsed() {
        // dfarq: readability copies the entire srcset string into src.
        let blob = "https://i0.wp.com/x.jpg?resize=300%2C158&ssl=1 300w, \
                    https://i0.wp.com/x.jpg?resize=1024%2C540&ssl=1 1024w, \
                    https://i0.wp.com/x.jpg?w=3000&ssl=1 3000w";
        let html = format!(r#"<img alt="printer" src="{blob}" srcset="{blob}"/>"#);
        // The widest candidate under the download ceiling wins; 3000w does not.
        assert_eq!(
            src_of(&normalize_img_tags(&html)),
            ["https://i0.wp.com/x.jpg?resize=1024%2C540&ssl=1"]
        );
    }

    #[test]
    fn a_json_blob_parked_in_src_is_rejected() {
        // Substack: readability copies data-attrs (JSON) over src.
        let html = r#"<img alt="" src="{&quot;src&quot;:&quot;https://s3.dev/a.jpeg&quot;,&quot;width&quot;:1000}"
            srcset="https://substackcdn.com/image/fetch/$s_!y1,w_1456,c_limit/a.jpeg 1456w"/>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://substackcdn.com/image/fetch/$s_!y1,w_1456,c_limit/a.jpeg"]
        );
    }

    #[test]
    fn picture_sources_back_up_an_empty_img() {
        let html = r#"<picture>
            <source srcset="https://cdn.dev/a.avif 800w" type="image/avif"/>
            <source srcset="https://cdn.dev/a.webp 800w" type="image/webp"/>
            <img alt="A photo"/>
        </picture>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://cdn.dev/a.avif"]
        );
    }

    #[test]
    fn a_good_src_is_never_traded_for_a_metadata_attribute() {
        // `data-template`, `data-attrs` and friends hold templates and JSON, not
        // URLs — trading a working src for one of those is the original sin.
        let html = r#"<img alt="A photo" loading="lazy" class="lazyload"
            src="https://cdn.dev/real.jpg"
            data-template="https://cdn.dev/{width}/real.jpg"
            data-orig-file="https://cdn.dev/orig.jpg"/>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://cdn.dev/real.jpg"]
        );
    }

    #[test]
    fn a_lazy_loader_placeholder_loses_to_its_data_src() {
        // iRunFar (a3-lazy-load): src is a shared 1×1 spacer that would download
        // and re-encode perfectly happily, and be the wrong picture.
        let html = r#"<img class="lazy lazy-hidden" alt="Brooks Cascadia 20"
            src="//www.irunfar.com/wp-content/plugins/a3-lazy-load/assets/images/lazy_placeholder.gif"
            data-src="https://s3.amazonaws.com/www.irunfar.com/uploads/Brooks-Cascadia-20.jpg"/>"#;
        assert_eq!(
            src_of(&normalize_img_tags(html)),
            ["https://s3.amazonaws.com/www.irunfar.com/uploads/Brooks-Cascadia-20.jpg"]
        );
    }

    #[test]
    fn normalization_keeps_alt_and_title_and_drops_the_rest() {
        let html = r#"<img src="/a.png" alt="An &amp; alt" title="T" class="x" width="900"
            onerror="evil()" srcset="/b.png 2x"/>"#;
        let out = normalize_img_tags(html);
        assert!(out.contains(r#"alt="An &amp; alt""#), "{out}");
        assert!(out.contains(r#"title="T""#), "{out}");
        for gone in ["class=", "width=", "onerror", "srcset="] {
            assert!(
                !out.contains(gone),
                "expected {gone} to be dropped from {out}"
            );
        }
        // Relative URLs survive: sanitize_with_base absolutizes them later.
        assert_eq!(src_of(&out), ["/a.png"]);
    }

    #[test]
    fn lightbox_buttons_no_longer_take_their_image_with_them() {
        // Nautilus: readability deletes <button> and everything inside it.
        let html = r#"<figure class="wp-block-image">
            <button type="button" aria-label="Enlarge image">
                <img class="wp-image-1" src="https://cdn.dev/flower.png?w=710" alt=""/>
            </button>
            <figcaption>BEAUTIFUL DANGER: a belladonna flower.</figcaption>
        </figure>"#;
        let out = unwrap_image_wrappers(html);
        assert!(!out.contains("<button"), "{out}");
        assert!(!out.contains("</button>"), "{out}");
        assert!(out.contains("flower.png"), "{out}");
        assert!(out.contains("BEAUTIFUL DANGER"), "caption survives: {out}");
    }

    #[test]
    fn buttons_that_are_really_buttons_are_left_alone() {
        let html = r#"<button class="subscribe">Subscribe <img src="/icon.png" alt=""/></button>"#;
        assert_eq!(unwrap_image_wrappers(html), html);
        // And an image-free control is untouched too.
        let plain = r#"<button>Share</button>"#;
        assert_eq!(unwrap_image_wrappers(plain), plain);
    }

    #[test]
    fn nested_and_unclosed_wrappers_do_not_derail_the_scan() {
        let nested = r#"<button><button><img src="/a.png"/></button></button><p>after</p>"#;
        let out = unwrap_image_wrappers(nested);
        assert!(!out.contains("button"), "{out}");
        assert!(out.contains("/a.png") && out.contains("after"), "{out}");
        // An open tag that never closes is passed through rather than eating
        // the rest of the document.
        let unclosed = r#"<button><img src="/a.png"/><p>rest</p>"#;
        assert!(unwrap_image_wrappers(unclosed).contains("rest"));
    }

    #[test]
    fn srcset_descriptors_pick_the_widest_usable_candidate() {
        assert_eq!(
            best_from_srcset("/a.png 150w, /b.png 800w, /c.png 4000w").as_deref(),
            Some("/b.png")
        );
        // Density descriptors work as an ordering too.
        assert_eq!(
            best_from_srcset("/a.png 1x, /b.png 2x").as_deref(),
            Some("/b.png")
        );
        // A bare URL with no descriptor is still a candidate.
        assert_eq!(best_from_srcset("/only.png").as_deref(), Some("/only.png"));
        // Every candidate too wide: take the narrowest rather than nothing.
        assert_eq!(
            best_from_srcset("/big.png 3000w, /huge.png 5000w").as_deref(),
            Some("/big.png")
        );
        assert_eq!(best_from_srcset("   ").as_deref(), None);
    }

    #[test]
    fn plausibility_is_about_shape_not_host() {
        for good in [
            "https://cdn.dev/a.jpg?id=1&width=980",
            "/_next/image?url=https%3A%2F%2Fx.dev%2Fa.png&w=3840&q=75",
            "https://substackcdn.com/image/fetch/$s_!y1,w_1456/a.jpeg",
        ] {
            assert!(plausible_url(good), "{good} should be usable");
        }
        for bad in [
            "",
            "data:image/svg+xml,%3Csvg%3E%3C/svg%3E",
            "https://cdn.dev/resize/{width}/a.jpg",
            r#"{"src":"https://cdn.dev/a.jpg"}"#,
            "https://cdn.dev/a.jpg 300w, https://cdn.dev/b.jpg 600w",
            "https://cdn.dev/a.jpg%20300w",
        ] {
            assert!(!plausible_url(bad), "{bad} should be rejected");
        }
    }
}
