//! Content extraction, sanitization and word counting (spec §3.3).
//!
//! Priority order per article: Miniflux content if it looks like full text →
//! fetch + `dom_smoothie` readability → feed excerpt with a "(excerpt only)" note.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use scraper::{Html, Node, Selector};
use url::Url;

use crate::epub::images::{tag_end, tag_name};
use crate::types::{Article, ExtractMethod, Extracted};

/// Word count at or above which Miniflux content is treated as full text (§3.3).
pub const FULL_TEXT_MIN_WORDS: i64 = 250;
/// Maximum bytes downloaded when fetching an article page (§3.3).
pub const MAX_FETCH_BYTES: usize = 3 * 1024 * 1024;
/// Note appended to bodies we could only excerpt (§3.3).
pub const EXCERPT_NOTE: &str = "(excerpt only — read online)";

/// Article pages are fetched with a desktop UA, not our bot UA (§3.3).
pub const DESKTOP_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
/// Per-fetch timeout for article pages (§3.3).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Parallel article fetches during the extraction stage.
pub const CONCURRENCY: usize = 8;
/// Below this many words, a page on a [`DEFAULT_PAYWALL_DOMAINS`] host is a stub (§3.3).
pub const PAYWALL_MAX_WORDS: i64 = 400;
/// Any page this short is an excerpt regardless of host (§3.3).
pub const EXCERPT_MAX_WORDS: i64 = 120;

/// Hosts that routinely serve a teaser instead of the article (§3.3).
///
/// The built-in list; `curation.paywall_domains` from the config file is merged
/// on top of it by [`Extractor::new`] / [`Extractor::offline`] (§3.3).
pub const DEFAULT_PAYWALL_DOMAINS: &[&str] = &[
    "nytimes.com",
    "wsj.com",
    "ft.com",
    "economist.com",
    "bloomberg.com",
    "washingtonpost.com",
    "newyorker.com",
    "theatlantic.com",
    "wired.com",
    "businessinsider.com",
    "barrons.com",
    "forbes.com",
    "latimes.com",
    "bostonglobe.com",
    "theinformation.com",
    "hbr.org",
    "nature.com",
    "science.org",
    "sciencedirect.com",
    "seekingalpha.com",
    "statnews.com",
    "thetimes.co.uk",
    "telegraph.co.uk",
    "medium.com",
    "towardsdatascience.com",
];

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("fetch failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("response exceeded {MAX_FETCH_BYTES} bytes")]
    TooLarge,
    #[error("readability found no main content")]
    NoContent,
    #[error("server returned {0}")]
    Status(u16),
    #[error("response was {0}, not html")]
    NotHtml(String),
    #[error("fetching is disabled on this extractor")]
    FetchDisabled,
}

/// Counters for the extraction stage, folded into the run report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractStats {
    pub from_miniflux: usize,
    pub from_readability: usize,
    pub excerpt_only: usize,
    pub fetch_failures: usize,
}

/// The extraction stage for one article (§3.3).
#[derive(Debug, Clone)]
pub struct Extractor {
    /// `None` disables the network path entirely (tests, `--dry-run` reruns).
    http: Option<reqwest::Client>,
    /// Hosts known to paywall, used by the [`looks_paywalled`] heuristic.
    paywall_domains: Vec<String>,
}

impl Extractor {
    pub fn new(http: reqwest::Client, paywall_domains: Vec<String>) -> Self {
        Self {
            http: Some(http),
            paywall_domains: merge_paywall_domains(paywall_domains),
        }
    }

    /// An extractor that never touches the network: the Miniflux/excerpt paths only.
    ///
    /// This is what tests use, and it keeps the fetch step injectable (§6 testing).
    pub fn offline(paywall_domains: Vec<String>) -> Self {
        Self {
            http: None,
            paywall_domains: merge_paywall_domains(paywall_domains),
        }
    }

    pub fn can_fetch(&self) -> bool {
        self.http.is_some()
    }

    /// Run the full priority order for one article and return its body (§3.3).
    ///
    /// Never fails the run: on fetch/readability failure it degrades to the feed
    /// excerpt (notes §3).
    pub async fn extract(&self, article: &Article) -> Extracted {
        let span = tracing::debug_span!("extract", entry = article.best_entry_id);
        let _guard = span.enter();

        // 1. Miniflux content, when it already looks like full text.
        let feed_html =
            sanitize_with_base(&normalize_img_tags(&article.content_html), &article.url);
        let feed_words = word_count(&feed_html);
        if feed_words >= FULL_TEXT_MIN_WORDS {
            return self.finish(
                article,
                feed_html,
                feed_words,
                ExtractMethod::Miniflux,
                &article.url,
            );
        }

        // 2. Fetch the page and run readability over it.
        if self.can_fetch() {
            match self.fetch_readable(&article.url).await {
                Ok(page) => {
                    // Relative URLs in the markup belong to the page we ended up
                    // on, not the one we asked for: shortener and syndication
                    // links land on another host entirely.
                    let clean =
                        sanitize_with_base(&normalize_img_tags(&page.html), &page.final_url);
                    let words = word_count(&clean);
                    if words > feed_words && words > 0 {
                        return self.finish(
                            article,
                            clean,
                            words,
                            ExtractMethod::Readability,
                            &page.final_url,
                        );
                    }
                    tracing::debug!(words, feed_words, "readability was not an improvement");
                }
                Err(e) => tracing::debug!(url = %article.url, "extraction fetch failed: {e}"),
            }
        }

        // 3. Excerpt fallback. Re-extraction must not stack up notes (notes §12).
        let body = if feed_html.trim().is_empty() {
            format!("<p>{EXCERPT_NOTE}</p>")
        } else if feed_html.contains(EXCERPT_NOTE) {
            feed_html
        } else {
            format!("{feed_html}<p>{EXCERPT_NOTE}</p>")
        };
        let words = word_count(&body);
        let mut out = self.finish(article, body, words, ExtractMethod::Excerpt, &article.url);
        out.excerpt_only = true;
        out
    }

    /// Assemble the [`Extracted`] value once a body has been chosen.
    ///
    /// `base_url` is what the body's relative URLs resolve against — the page we
    /// landed on for a fetched article, the article URL otherwise.
    fn finish(
        &self,
        article: &Article,
        content_html: String,
        words: i64,
        method: ExtractMethod,
        base_url: &str,
    ) -> Extracted {
        let image_urls = collect_image_urls(&content_html, base_url);
        let excerpt_only = method == ExtractMethod::Excerpt
            || looks_paywalled(&article.url, words, &self.paywall_domains);
        Extracted {
            content_html,
            word_count: words,
            excerpt_only,
            image_urls,
            method,
        }
    }

    /// Extract every article in place, up to [`CONCURRENCY`] fetches at a time (§3.3).
    pub async fn extract_all(&self, articles: &mut [Article]) -> ExtractStats {
        let span = tracing::info_span!("extract_all", articles = articles.len());
        let _guard = span.enter();

        let inputs: Vec<Article> = articles.to_vec();
        let results: Vec<(usize, Extracted)> = futures::stream::iter(inputs.iter().enumerate())
            .map(|(i, article)| async move { (i, self.extract(article).await) })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;

        let mut stats = ExtractStats::default();
        for (i, extracted) in results {
            match extracted.method {
                ExtractMethod::Miniflux => stats.from_miniflux += 1,
                ExtractMethod::Readability => stats.from_readability += 1,
                ExtractMethod::Excerpt => stats.fetch_failures += 1,
            }
            if extracted.excerpt_only {
                stats.excerpt_only += 1;
            }
            apply(&mut articles[i], extracted);
        }
        tracing::info!(
            miniflux = stats.from_miniflux,
            readability = stats.from_readability,
            excerpt_only = stats.excerpt_only,
            "extraction complete"
        );
        stats
    }

    /// Fetch `url` (10s timeout, desktop UA, [`MAX_FETCH_BYTES`] cap) and run
    /// `dom_smoothie` readability over it (§3.3).
    ///
    /// Returns the URL the fetch actually landed on alongside the markup, so
    /// callers resolve relative links against the right origin.
    pub async fn fetch_readable(&self, url: &str) -> Result<Page, ExtractError> {
        let Some(http) = &self.http else {
            return Err(ExtractError::FetchDisabled);
        };
        let mut response = http
            .get(url)
            .header(reqwest::header::USER_AGENT, DESKTOP_UA)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .timeout(FETCH_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ExtractError::Status(response.status().as_u16()));
        }
        let final_url = response.url().to_string();
        if let Some(ct) = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            let ct = ct.to_ascii_lowercase();
            if !(ct.contains("html") || ct.contains("xml") || ct.contains("text/plain")) {
                return Err(ExtractError::NotHtml(ct));
            }
        }

        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_FETCH_BYTES {
                return Err(ExtractError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let html = String::from_utf8_lossy(&body).into_owned();
        Ok(Page {
            html: readability(&html, &final_url)?,
            final_url,
        })
    }
}

/// An article page after fetching and readability (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Readability's main-content markup.
    pub html: String,
    /// Where the fetch ended up, after any redirects — the base for relative URLs.
    pub final_url: String,
}

/// Run `dom_smoothie` over a fetched page and return its main-content HTML (§3.3).
///
/// The page is normalized first ([`prepare_for_readability`]) so that readability
/// sees plain, already-resolved `<img src>` elements. Left to itself it damages
/// them in two ways: it deletes whole subtrees (lightbox `<button>` wrappers take
/// their image with them) and its lazy-image heuristic overwrites a perfectly
/// good `src` with whatever other attribute happens to contain `.jpg`.
pub fn readability(html: &str, url: &str) -> Result<String, ExtractError> {
    let html = prepare_for_readability(html);
    let config = dom_smoothie::Config {
        max_elements_to_parse: 60_000,
        ..Default::default()
    };
    let mut readability = dom_smoothie::Readability::new(html.as_str(), Some(url), Some(config))
        .map_err(|_| ExtractError::NoContent)?;
    let parsed = readability.parse().map_err(|_| ExtractError::NoContent)?;
    let content = parsed.content.to_string();
    if content.trim().is_empty() {
        return Err(ExtractError::NoContent);
    }
    Ok(content)
}

/// Copy an [`Extracted`] onto its [`Article`].
pub fn apply(article: &mut Article, extracted: Extracted) {
    article.image_count = extracted.image_urls.len() as i64;
    article.image_urls = extracted.image_urls;
    article.content_html = extracted.content_html;
    article.word_count = extracted.word_count;
    article.excerpt_only = extracted.excerpt_only;
    article.extract_method = extracted.method;
}

fn merge_paywall_domains(configured: Vec<String>) -> Vec<String> {
    let mut domains: Vec<String> = DEFAULT_PAYWALL_DOMAINS
        .iter()
        .map(|d| (*d).to_string())
        .collect();
    for d in configured {
        let d = d.trim().to_ascii_lowercase();
        if !d.is_empty() && !domains.contains(&d) {
            domains.push(d);
        }
    }
    domains
}

// ---------------------------------------------------------------------------
// Image normalization (§3.3)
// ---------------------------------------------------------------------------

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
                let attrs = crate::epub::images::parse_attrs(inner);
                if picture_srcset.is_none()
                    && let Some(set) =
                        attr(&attrs, "srcset").or_else(|| attr(&attrs, "data-srcset"))
                {
                    picture_srcset = Some(set.to_string());
                }
                out.push_str(raw);
            }
            "img" => {
                let attrs = crate::epub::images::parse_attrs(inner);
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
                    tracing::debug!(tag = %&raw[..raw.len().min(120)], "dropping unusable img");
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

// ---------------------------------------------------------------------------
// Sanitization (§3.3)
// ---------------------------------------------------------------------------

/// Tags the EPUB templates accept (§3.3).
pub const ALLOWED_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "em",
    "strong",
    "a",
    "img",
    "figure",
    "figcaption",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "caption",
    "hr",
    "br",
];

fn builder() -> ammonia::Builder<'static> {
    let mut tag_attributes: std::collections::HashMap<&str, HashSet<&str>> =
        std::collections::HashMap::new();
    tag_attributes.insert("a", ["href", "title"].into_iter().collect());
    tag_attributes.insert("img", ["src", "alt", "title"].into_iter().collect());
    tag_attributes.insert("th", ["colspan", "rowspan", "scope"].into_iter().collect());
    tag_attributes.insert("td", ["colspan", "rowspan"].into_iter().collect());

    let mut b = ammonia::Builder::default();
    b.tags(ALLOWED_TAGS.iter().copied().collect())
        .tag_attributes(tag_attributes)
        .generic_attributes(HashSet::new())
        .link_rel(None)
        .strip_comments(true);
    b
}

/// Sanitize to the safe XHTML subset the EPUB templates allow (§3.3).
///
/// Allowed: `p`, `h1`–`h4`, `ul`/`ol`/`li`, `blockquote`, `pre`, `code`, `em`,
/// `strong`, `a`, `img`, `figure`, `figcaption`, table basics, `hr`, `br`.
pub fn sanitize(html: &str) -> String {
    builder().clean(html).to_string()
}

/// [`sanitize`], additionally rewriting relative `href`/`src` against `base_url`
/// so the EPUB (which has no base URL) still resolves them (§3.3).
pub fn sanitize_with_base(html: &str, base_url: &str) -> String {
    match Url::parse(base_url) {
        Ok(base) => builder()
            .url_relative(ammonia::UrlRelative::RewriteWithBase(base))
            .clean(html)
            .to_string(),
        Err(_) => sanitize(html),
    }
}

// ---------------------------------------------------------------------------
// Text measurement (§3.3)
// ---------------------------------------------------------------------------

/// Visible text of an HTML fragment, entities decoded, `script`/`style` skipped.
pub fn html_to_text(html: &str) -> String {
    let document = Html::parse_fragment(html);
    let mut out = String::with_capacity(html.len() / 2);
    for node in document.tree.nodes() {
        let Node::Text(text) = node.value() else {
            continue;
        };
        let hidden = node.ancestors().any(|a| match a.value() {
            Node::Element(e) => matches!(e.name(), "script" | "style" | "noscript"),
            _ => false,
        });
        if hidden {
            continue;
        }
        out.push_str(text);
        out.push(' ');
    }
    out
}

/// Count words in rendered text (tags stripped) (§3.3).
pub fn word_count(html: &str) -> i64 {
    html_to_text(html)
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count() as i64
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

/// Heuristic paywall detection: very short text on a known paywall domain (§3.3).
///
/// Two rules: anything under [`EXCERPT_MAX_WORDS`] is a stub whatever the host,
/// and anything under [`PAYWALL_MAX_WORDS`] on a `paywall_domains` host is a teaser.
pub fn looks_paywalled(url: &str, word_count: i64, paywall_domains: &[String]) -> bool {
    if word_count <= 0 {
        return true;
    }
    if word_count < EXCERPT_MAX_WORDS {
        return true;
    }
    if word_count >= PAYWALL_MAX_WORDS {
        return false;
    }
    let Some(host) = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
    else {
        return false;
    };
    paywall_domains
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Shared-ownership helper for callers that want one extractor across tasks.
pub fn shared(extractor: Extractor) -> Arc<Extractor> {
    Arc::new(extractor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SourceKind;
    use jiff::Timestamp;

    fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z".parse().unwrap()
    }

    fn article(url: &str, content: &str) -> Article {
        Article {
            id: 0,
            canonical_url: url.into(),
            title: "T".into(),
            best_entry_id: 1,
            content_html: content.into(),
            word_count: 0,
            excerpt_only: false,
            image_count: 0,
            sources: vec![crate::types::SourceRef {
                entry_id: 1,
                feed_id: 1,
                feed_title: "Feed".into(),
                category: None,
                kind: SourceKind::Feed,
            }],
            first_seen: ts(),
            url: url.into(),
            author: None,
            feed_id: 1,
            feed_title: "Feed".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        }
    }

    fn long_body(words: usize) -> String {
        format!("<p>{}</p>", "lorem ".repeat(words))
    }

    #[test]
    fn sanitize_keeps_the_allowlist_and_drops_everything_else() {
        let dirty = r#"
            <h1>Title</h1><h5>too deep</h5>
            <p class="x" onclick="evil()">Hello <em>there</em> <strong>you</strong></p>
            <script>alert(1)</script><style>p{color:red}</style>
            <div><span>unwrapped</span></div>
            <ul><li>one</li></ul><ol><li>two</li></ol>
            <blockquote>quote</blockquote><pre><code>fn main() {}</code></pre>
            <table><thead><tr><th>h</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>
            <figure><img src="https://x.dev/a.png" alt="a" width="10"><figcaption>cap</figcaption></figure>
            <a href="https://x.dev" target="_blank" rel="nofollow">link</a>
            <a href="javascript:alert(1)">bad</a>
            <iframe src="https://evil.dev"></iframe><hr><br>
            <!-- comment -->
        "#;
        let clean = sanitize(dirty);

        for keep in [
            "<h1>",
            "<p>",
            "<em>",
            "<strong>",
            "<ul>",
            "<li>",
            "<ol>",
            "<blockquote>",
            "<pre>",
            "<code>",
            "<table>",
            "<th>",
            "<td>",
            "<figure>",
            "<figcaption>",
            "<hr",
            "<br",
        ] {
            assert!(clean.contains(keep), "expected {keep} in {clean}");
        }
        assert!(clean.contains(r#"src="https://x.dev/a.png""#));
        assert!(clean.contains(r#"alt="a""#));
        assert!(clean.contains(r#"href="https://x.dev""#));

        for drop in [
            "<h5",
            "<script",
            "<style",
            "alert(1)",
            "<div",
            "<span",
            "<iframe",
            "onclick",
            "class=",
            "width=",
            "javascript:",
            "<!--",
        ] {
            assert!(!clean.contains(drop), "did not expect {drop} in {clean}");
        }
        // Text inside stripped containers survives; the tags do not.
        assert!(clean.contains("unwrapped"));
    }

    #[test]
    fn sanitize_with_base_absolutizes_urls() {
        let html = r#"<p><a href="/next">n</a><img src="img/a.png" alt="a"></p>"#;
        let clean = sanitize_with_base(html, "https://blog.dev/posts/one");
        assert!(clean.contains(r#"href="https://blog.dev/next""#));
        assert!(clean.contains(r#"src="https://blog.dev/posts/img/a.png""#));
        // A bad base degrades to plain sanitization rather than failing.
        assert!(sanitize_with_base(html, "not a url").contains("/next"));
    }

    #[test]
    fn word_count_ignores_markup_and_script() {
        assert_eq!(word_count("<p>one two three</p>"), 3);
        assert_eq!(word_count("<p>a</p><script>b c d e</script>"), 1);
        assert_eq!(word_count("<p>&amp; &mdash; ok</p>"), 1);
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("<p></p>"), 0);
    }

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

    #[tokio::test]
    async fn feed_content_is_normalized_too() {
        // Feeds carry the same lazy markup pages do.
        let extractor = Extractor::offline(vec![]);
        let body = format!(
            r#"<p>{}</p><img src="data:image/gif;base64,zz" data-src="https://cdn.dev/real.jpg"/>"#,
            "lorem ".repeat(400)
        );
        let article = article("https://blog.dev/p", &body);
        let out = extractor.extract(&article).await;
        assert_eq!(out.method, ExtractMethod::Miniflux);
        assert_eq!(out.image_urls, ["https://cdn.dev/real.jpg"]);
    }

    #[test]
    fn paywall_heuristic() {
        let domains = merge_paywall_domains(vec!["paywalled.dev".into()]);
        // Known paywall host with a stub body.
        assert!(looks_paywalled("https://www.nytimes.com/x", 200, &domains));
        assert!(looks_paywalled("https://paywalled.dev/x", 200, &domains));
        // Same host, full article.
        assert!(!looks_paywalled(
            "https://www.nytimes.com/x",
            1500,
            &domains
        ));
        // Unknown host with a normal-length body.
        assert!(!looks_paywalled("https://blog.dev/x", 200, &domains));
        // Anything this short is an excerpt no matter the host.
        assert!(looks_paywalled("https://blog.dev/x", 40, &domains));
        assert!(looks_paywalled("https://blog.dev/x", 0, &domains));
        // Unparseable URLs never claim a paywall on their own.
        assert!(!looks_paywalled("nonsense", 900, &domains));
    }

    #[tokio::test]
    async fn miniflux_content_wins_when_it_is_full_text() {
        let extractor = Extractor::offline(vec![]);
        let article = article("https://blog.dev/p", &long_body(600));
        let out = extractor.extract(&article).await;
        assert_eq!(out.method, ExtractMethod::Miniflux);
        assert!(out.word_count >= FULL_TEXT_MIN_WORDS);
        assert!(!out.excerpt_only);
    }

    #[tokio::test]
    async fn short_content_falls_back_to_the_excerpt_note() {
        let extractor = Extractor::offline(vec![]);
        let stub = article("https://blog.dev/p", "<p>Just a teaser.</p>");
        let out = extractor.extract(&stub).await;
        assert_eq!(out.method, ExtractMethod::Excerpt);
        assert!(out.excerpt_only);
        assert!(out.content_html.contains(EXCERPT_NOTE));
        assert!(out.content_html.contains("Just a teaser."));

        // Empty feed content still yields a body, never a panic.
        let empty = article("https://blog.dev/p", "");
        let out = extractor.extract(&empty).await;
        assert_eq!(out.method, ExtractMethod::Excerpt);
        assert!(out.content_html.contains(EXCERPT_NOTE));
    }

    #[tokio::test]
    async fn extract_all_applies_results_and_counts() {
        let extractor = Extractor::offline(vec![]);
        let mut articles = vec![
            article("https://blog.dev/full", &long_body(600)),
            article("https://blog.dev/stub", "<p>teaser</p>"),
        ];
        articles[0]
            .content_html
            .push_str(r#"<p><img src="/pic.png" alt="p"></p>"#);

        let stats = extractor.extract_all(&mut articles).await;
        assert_eq!(stats.from_miniflux, 1);
        assert_eq!(stats.excerpt_only, 1);
        assert_eq!(articles[0].extract_method, ExtractMethod::Miniflux);
        assert_eq!(articles[0].image_count, 1);
        assert_eq!(articles[0].image_urls, ["https://blog.dev/pic.png"]);
        assert!(articles[0].word_count >= 600);
        assert_eq!(articles[1].extract_method, ExtractMethod::Excerpt);
        assert!(articles[1].excerpt_only);
    }

    /// A redirected article's relative image URLs belong to where it landed.
    ///
    /// `postgr.es/p/9sl` redirects to `boringsql.com`; resolving its `/images/…`
    /// against the shortener produced two 404s and two lost charts.
    #[tokio::test]
    async fn relative_urls_resolve_against_the_url_we_landed_on() {
        use axum::response::{Html, Redirect};
        use axum::routing::get;

        let page = format!(
            "<html><body><article><h1>Post</h1>{}\
             <img src=\"images/chart.svg\" alt=\"A chart\"/></article></body></html>",
            "<p>Body copy that readability will happily keep. </p>".repeat(40)
        );
        let app = axum::Router::new()
            .route("/p/9sl", get(|| async { Redirect::to("/posts/real/") }))
            .route("/posts/real/", get(move || async move { Html(page) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let http = crate::http::build_client(Duration::from_secs(5)).unwrap();
        let extractor = Extractor::new(http, vec![]);
        let article = article(&format!("http://{addr}/p/9sl"), "<p>stub</p>");
        let out = extractor.extract(&article).await;
        server.abort();

        assert_eq!(out.method, ExtractMethod::Readability);
        assert_eq!(
            out.image_urls,
            [format!("http://{addr}/posts/real/images/chart.svg")],
            "the shortener path must not be the base"
        );
    }

    #[tokio::test]
    async fn offline_extractor_never_fetches() {
        let extractor = Extractor::offline(vec![]);
        assert!(!extractor.can_fetch());
        assert!(matches!(
            extractor.fetch_readable("https://blog.dev/p").await,
            Err(ExtractError::FetchDisabled)
        ));
    }

    #[test]
    fn readability_pulls_the_main_content_out_of_a_page() {
        let paragraph = "Readability keeps the body copy and throws away the chrome. ".repeat(20);
        let html = format!(
            "<html><head><title>A Post</title></head><body>\
             <nav><a href=\"/\">home</a></nav>\
             <article><h1>A Post</h1><p>{paragraph}</p><p>{paragraph}</p></article>\
             <footer>© 2026</footer></body></html>"
        );
        let content = readability(&html, "https://blog.dev/p").expect("main content");
        assert!(content.contains("Readability keeps the body copy"));
        let clean = sanitize_with_base(&content, "https://blog.dev/p");
        assert!(word_count(&clean) > 200);
        assert!(!clean.contains("<nav"));
    }
}
