//! Finding the images an article references.
//!
//! The input here is already-normalized markup (see [`super::normalize`]), so
//! every `<img>` is expected to carry a plain, usable `src`.

use std::collections::HashSet;

use scraper::{Html, Selector};
use url::Url;

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

/// Absolute image URLs referenced by `html`, resolved against `base_url` (§3.3).
///
/// Every image an article carries is kept: a photo essay with thirty pictures is
/// a photo essay, and the issue-wide byte budget is the real backstop.
pub fn collect_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse("img") else {
        return Vec::new();
    };
    let base = Url::parse(base_url).ok();
    let document = Html::parse_fragment(html);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for element in document.select(&selector) {
        let raw = element
            .value()
            .attr("src")
            .or_else(|| element.value().attr("data-src"))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(raw) = raw else { continue };
        let resolved = match Url::parse(raw) {
            Ok(u) => Some(u),
            Err(_) => base.as_ref().and_then(|b| b.join(raw).ok()),
        };
        let Some(url) = resolved.filter(|u| matches!(u.scheme(), "http" | "https")) else {
            continue;
        };
        let url = url.to_string();
        if seen.insert(url.clone()) {
            out.push(url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_collection_resolves_and_keeps_every_image() {
        let mut html = String::from(r#"<img src="/a.png"><img src="https://cdn.dev/b.png">"#);
        html.push_str(r#"<img data-src="c.png"><img src="/a.png"><img src="data:image/png;x">"#);
        for i in 0..20 {
            html.push_str(&format!(r#"<img src="/n{i}.png">"#));
        }
        let urls = collect_image_urls(&html, "https://blog.dev/posts/one");
        // Two named images, the data-src one, and all twenty of the rest: no cap.
        assert_eq!(urls.len(), 23);
        assert_eq!(urls[0], "https://blog.dev/a.png");
        assert_eq!(urls[1], "https://cdn.dev/b.png");
        assert_eq!(urls[2], "https://blog.dev/posts/c.png");
        // Duplicates and data: URIs never appear.
        assert_eq!(urls.iter().filter(|u| u.ends_with("/a.png")).count(), 1);
        assert!(!urls.iter().any(|u| u.starts_with("data:")));
        assert!(collect_image_urls("<p>none</p>", "https://blog.dev").is_empty());
    }

    // -----------------------------------------------------------------------
    // Image normalization — each case is a page shape seen in a real issue.
    // -----------------------------------------------------------------------

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
}
