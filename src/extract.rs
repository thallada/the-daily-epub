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

use crate::types::{Article, ExtractMethod, Extracted};

/// Word count at or above which Miniflux content is treated as full text (§3.3).
pub const FULL_TEXT_MIN_WORDS: i64 = 250;
/// Maximum bytes downloaded when fetching an article page (§3.3).
pub const MAX_FETCH_BYTES: usize = 3 * 1024 * 1024;
/// Maximum images collected per article (§3.3).
pub const MAX_IMAGES_PER_ARTICLE: usize = 12;
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
        let feed_html = sanitize_with_base(&article.content_html, &article.url);
        let feed_words = word_count(&feed_html);
        if feed_words >= FULL_TEXT_MIN_WORDS {
            return self.finish(article, feed_html, feed_words, ExtractMethod::Miniflux);
        }

        // 2. Fetch the page and run readability over it.
        if self.can_fetch() {
            match self.fetch_readable(&article.url).await {
                Ok(html) => {
                    let clean = sanitize_with_base(&html, &article.url);
                    let words = word_count(&clean);
                    if words > feed_words && words > 0 {
                        return self.finish(article, clean, words, ExtractMethod::Readability);
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
        let mut out = self.finish(article, body, words, ExtractMethod::Excerpt);
        out.excerpt_only = true;
        out
    }

    /// Assemble the [`Extracted`] value once a body has been chosen.
    fn finish(
        &self,
        article: &Article,
        content_html: String,
        words: i64,
        method: ExtractMethod,
    ) -> Extracted {
        let image_urls = collect_image_urls(&content_html, &article.url);
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
    pub async fn fetch_readable(&self, url: &str) -> Result<String, ExtractError> {
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
        readability(&html, url)
    }
}

/// Run `dom_smoothie` over a fetched page and return its main-content HTML (§3.3).
pub fn readability(html: &str, url: &str) -> Result<String, ExtractError> {
    let config = dom_smoothie::Config {
        max_elements_to_parse: 60_000,
        ..Default::default()
    };
    let mut readability = dom_smoothie::Readability::new(html, Some(url), Some(config))
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

/// Absolute image URLs referenced by `html`, resolved against `base_url`,
/// capped at [`MAX_IMAGES_PER_ARTICLE`] (§3.3).
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
        if out.len() >= MAX_IMAGES_PER_ARTICLE {
            break;
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
    fn image_collection_resolves_and_caps() {
        let mut html = String::from(r#"<img src="/a.png"><img src="https://cdn.dev/b.png">"#);
        html.push_str(r#"<img data-src="c.png"><img src="/a.png"><img src="data:image/png;x">"#);
        for i in 0..20 {
            html.push_str(&format!(r#"<img src="/n{i}.png">"#));
        }
        let urls = collect_image_urls(&html, "https://blog.dev/posts/one");
        assert_eq!(urls.len(), MAX_IMAGES_PER_ARTICLE);
        assert_eq!(urls[0], "https://blog.dev/a.png");
        assert_eq!(urls[1], "https://cdn.dev/b.png");
        assert_eq!(urls[2], "https://blog.dev/posts/c.png");
        // Duplicates and data: URIs never appear.
        assert_eq!(urls.iter().filter(|u| u.ends_with("/a.png")).count(), 1);
        assert!(!urls.iter().any(|u| u.starts_with("data:")));
        assert!(collect_image_urls("<p>none</p>", "https://blog.dev").is_empty());
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
