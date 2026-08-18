//! Pointing article markup at the images actually embedded in the EPUB.

use std::collections::HashMap;

use crate::html::{attr_escape, parse_attrs, tag_end, tag_name, text_escape};
use crate::types::ImageAsset;

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
}
