//! World Briefing from the Wikipedia Current Events portal (spec §3.8).
//!
//! Failure is non-fatal: the section is simply omitted.

use std::collections::HashSet;

use jiff::civil::Date;
use scraper::node::Node;

use crate::epub::images::{text_escape, to_xhtml};
use crate::types::WorldBriefing;

/// `ego_tree::NodeRef<'_, Node>` without depending on `ego_tree` directly.
type NodeRef<'a> = <scraper::ElementRef<'a> as std::ops::Deref>::Target;

/// Portal page pattern: `Portal:Current_events/{YYYY}_{Month}_{D}` (§3.8).
pub const PORTAL_BASE: &str = "https://en.wikipedia.org/wiki/Portal:Current_events/";
/// MediaWiki REST HTML endpoint used to fetch the rendered page (§3.8).
pub const REST_HTML_BASE: &str = "https://en.wikipedia.org/api/rest_v1/page/html/";
/// Attribution line required by the portal's licence (§3.8).
pub const ATTRIBUTION: &str = "Source: Wikipedia Current Events Portal, CC BY-SA 4.0.";
/// How many days back [`fetch_with_fallback`] will look for a populated page.
///
/// The portal page for a day is created as an empty stub a day ahead and filled
/// in over the course of that day, so the 05:30 run finds nothing under the
/// issue's own date. Walking back one or two days lands on a complete page —
/// which is also the news the reader has not seen yet at breakfast.
pub const MAX_LOOKBACK_DAYS: i8 = 3;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Containers the day's events live in, most specific first (§3.8).
const CONTENT_SELECTORS: &[&str] = &[
    "div.current-events-content",
    "div.description",
    "div.current-events-main",
    "section",
    "body",
];

/// Elements whose entire subtree is dropped: citations, edit links, chrome.
const DROP_ELEMENTS: &[&str] = &[
    "script", "style", "sup", "table", "figure", "img", "link", "meta", "noscript", "input",
    "button", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// Class fragments marking wiki chrome rather than content.
const DROP_CLASSES: &[&str] = &[
    "mw-editsection",
    "reference",
    "navbox",
    "metadata",
    "noprint",
    "current-events-navbar",
    "current-events-heading",
    "hatnote",
    "mw-jump-link",
];

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no events found for {0}")]
    Empty(Date),
}

/// Build the portal page title for a date, e.g. `2026_August_15` (§3.8).
pub fn portal_title(date: Date) -> String {
    let month = MONTHS
        .get((date.month() as usize).saturating_sub(1))
        .copied()
        .unwrap_or("January");
    format!("{}_{}_{}", date.year(), month, date.day())
}

/// Human-readable portal URL, used for the CC BY-SA attribution link (§3.8).
pub fn portal_url(date: Date) -> String {
    format!("{PORTAL_BASE}{}", portal_title(date))
}

/// MediaWiki REST HTML URL for the day's portal page (§3.8).
pub fn rest_html_url(date: Date) -> String {
    format!(
        "{REST_HTML_BASE}Portal%3ACurrent_events%2F{}",
        portal_title(date)
    )
}

/// Fetch the day's portal page, strip citations/edit links, flatten internal
/// links to plain text and return a compact briefing (§3.8).
pub async fn fetch(http: &reqwest::Client, date: Date) -> Result<WorldBriefing, WorldError> {
    let url = rest_html_url(date);
    tracing::debug!(%url, "fetching the world briefing");
    let html = http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let body_html = extract_events(&html).ok_or(WorldError::Empty(date))?;
    Ok(WorldBriefing {
        date,
        source_url: portal_url(date),
        body_html,
    })
}

/// The days [`fetch_with_fallback`] tries, newest first: `date`, then each
/// earlier day up to `max_days_back` (§3.8).
pub fn candidate_days(date: Date, max_days_back: i8) -> Vec<Date> {
    (0..=max_days_back.max(0))
        .map_while(|back| date.checked_sub(jiff::Span::new().days(back)).ok())
        .collect()
}

/// Fetch the newest populated portal page at or before `date`, looking back at
/// most [`MAX_LOOKBACK_DAYS`] days (§3.8).
///
/// The issue's own day is almost always still an empty stub at 05:30, so this is
/// the entry point the pipeline uses; the returned briefing carries the date it
/// actually covers in [`WorldBriefing::date`].
pub async fn fetch_with_fallback(
    http: &reqwest::Client,
    date: Date,
    max_days_back: i8,
) -> Result<WorldBriefing, WorldError> {
    let mut last = WorldError::Empty(date);
    for day in candidate_days(date, max_days_back) {
        match fetch(http, day).await {
            Ok(briefing) => {
                if day != date {
                    tracing::info!(
                        %date,
                        covering = %day,
                        "the issue day's portal page was not populated yet; using an earlier day"
                    );
                }
                return Ok(briefing);
            }
            Err(e) => {
                tracing::debug!(%day, "world briefing not available for this day: {e}");
                last = e;
            }
        }
    }
    Err(last)
}

/// Best-effort wrapper used by the pipeline: never fails the run (§3.8).
pub async fn fetch_optional(
    http: &reqwest::Client,
    date: Date,
    enabled: bool,
) -> Option<WorldBriefing> {
    if !enabled {
        return None;
    }
    match fetch_with_fallback(http, date, MAX_LOOKBACK_DAYS).await {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(%date, "world briefing unavailable: {e}");
            None
        }
    }
}

/// Extract the day's bulleted events from a rendered portal page (§3.8).
///
/// Citations, edit links and navigation are dropped; internal links become plain
/// text; the result is a sanitized `<p>`/`<ul>` fragment.
pub fn extract_events(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    for selector in CONTENT_SELECTORS {
        let Ok(sel) = scraper::Selector::parse(selector) else {
            continue;
        };
        for container in doc.select(&sel) {
            let mut out = String::new();
            walk_children(*container, &mut out);
            let cleaned = sanitize(&out);
            if !cleaned.is_empty() && cleaned.contains("<li>") {
                return Some(to_xhtml(&cleaned));
            }
        }
    }
    None
}

fn sanitize(fragment: &str) -> String {
    let tags: HashSet<&str> = ["p", "ul", "ol", "li", "strong", "em", "br"]
        .into_iter()
        .collect();
    ammonia::Builder::new()
        .tags(tags)
        .clean(fragment)
        .to_string()
        .trim()
        .to_string()
}

fn is_dropped(el: &scraper::node::Element) -> bool {
    if DROP_ELEMENTS.contains(&el.name()) {
        return true;
    }
    if let Some(class) = el.attr("class")
        && DROP_CLASSES
            .iter()
            .any(|dropped| class.split_whitespace().any(|c| c == *dropped))
    {
        return true;
    }
    if el.attr("role") == Some("navigation") {
        return true;
    }
    false
}

fn walk_children(node: NodeRef<'_>, out: &mut String) {
    for child in node.children() {
        walk(child, out);
    }
}

fn walk(node: NodeRef<'_>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&text_escape(text)),
        Node::Element(el) => {
            if is_dropped(el) {
                return;
            }
            match el.name() {
                "ul" | "ol" | "li" | "p" => {
                    let name = el.name();
                    out.push('<');
                    out.push_str(name);
                    out.push('>');
                    walk_children(node, out);
                    out.push_str("</");
                    out.push_str(name);
                    out.push('>');
                }
                "b" | "strong" => {
                    out.push_str("<strong>");
                    walk_children(node, out);
                    out.push_str("</strong>");
                }
                "i" | "em" => {
                    out.push_str("<em>");
                    walk_children(node, out);
                    out.push_str("</em>");
                }
                "dt" => {
                    out.push_str("<p><strong>");
                    walk_children(node, out);
                    out.push_str("</strong></p>");
                }
                "br" => out.push(' '),
                // `a`, `span`, `div`, `dl`, `dd`, `section` … are transparent:
                // internal links keep their text only (§3.8).
                _ => walk_children(node, out),
            }
        }
        _ => {}
    }
}

/// Render the briefing to sanitized XHTML with the CC BY-SA attribution (§3.8).
pub fn render_xhtml(briefing: &WorldBriefing) -> String {
    format!(
        "{}\n      <p class=\"attribution\">{} <a href=\"{}\">{}</a></p>\n",
        briefing.body_html,
        text_escape(ATTRIBUTION),
        text_escape(&briefing.source_url),
        text_escape(&briefing.source_url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/wikipedia_current_events.html");
        std::fs::read_to_string(path).expect("fixture must exist")
    }

    #[test]
    fn portal_titles_and_urls_match_the_spec() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(portal_title(date), "2026_August_15");
        assert_eq!(
            portal_url(date),
            "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_15"
        );
        assert_eq!(
            rest_html_url(date),
            "https://en.wikipedia.org/api/rest_v1/page/html/Portal%3ACurrent_events%2F2026_August_15"
        );
        let single_digit: Date = "2026-01-05".parse().unwrap();
        assert_eq!(portal_title(single_digit), "2026_January_5");
    }

    #[test]
    fn extracts_events_and_strips_wiki_chrome() {
        let body = extract_events(&fixture()).expect("events");
        assert!(body.contains("<ul>"));
        assert!(body.contains("<strong>Armed conflicts and attacks</strong>"));
        assert!(body.contains("Heavy rain floods the Charles River basin"));
        // Internal links are flattened to plain text.
        assert!(!body.contains("<a"));
        assert!(body.contains("Boston"));
        // Citations, edit links and navboxes are gone.
        assert!(!body.contains("[1]"));
        assert!(!body.contains("edit"));
        assert!(!body.contains("Ongoing events"));
        // Nested sub-bullets survive.
        assert!(body.contains("A second-level detail"));
    }

    #[test]
    fn missing_events_yield_none() {
        assert!(extract_events("<html><body><p>Nothing here</p></body></html>").is_none());
        assert!(extract_events("").is_none());
    }

    /// Wikipedia creates each day's portal page as an empty stub a day ahead and
    /// fills it in over that day, so the 05:30 run sees this, not news (§3.8).
    /// Its only `<li>`s are the edit/history/watch navbar, which must not count
    /// as content — otherwise the fallback never triggers.
    #[test]
    fn an_unpopulated_stub_page_yields_no_events() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/wikipedia_current_events_empty_stub.html");
        let stub = std::fs::read_to_string(path).expect("fixture must exist");
        assert!(stub.contains("current-events-navbar"), "fixture sanity");
        assert!(extract_events(&stub).is_none());
    }

    #[test]
    fn the_fallback_walks_backwards_from_the_issue_date() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(
            candidate_days(date, 3),
            [
                "2026-08-15".parse().unwrap(),
                "2026-08-14".parse().unwrap(),
                "2026-08-13".parse().unwrap(),
                "2026-08-12".parse().unwrap(),
            ]
        );
        // Never forwards, and never fewer than the issue's own day.
        assert_eq!(candidate_days(date, 0), [date]);
        assert_eq!(candidate_days(date, -1), [date]);
        // Month and year boundaries.
        assert_eq!(
            candidate_days("2026-01-01".parse().unwrap(), 2),
            [
                "2026-01-01".parse().unwrap(),
                "2025-12-31".parse().unwrap(),
                "2025-12-30".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn rendering_appends_the_attribution() {
        let briefing = WorldBriefing {
            date: "2026-08-15".parse().unwrap(),
            source_url: portal_url("2026-08-15".parse().unwrap()),
            body_html: "<ul><li>Something happened</li></ul>".into(),
        };
        let xhtml = render_xhtml(&briefing);
        assert!(xhtml.contains("CC BY-SA 4.0"));
        assert!(xhtml.contains(
            "<a href=\"https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_15\">"
        ));
        assert!(xhtml.contains("Something happened"));
    }
}
