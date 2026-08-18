//! Structured, best-effort World Briefing from Wikipedia Current Events.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};
use jiff::civil::Date;
use scraper::{ElementRef, node::Node};
use serde::Deserialize;
use url::Url;

use crate::curate::llm::{LlmClient, LlmError};
use crate::html::text_escape;
use crate::types::{WorldBriefing, WorldBriefingSection, WorldEvent};

type NodeRef<'a> = <scraper::ElementRef<'a> as std::ops::Deref>::Target;

pub const PORTAL_BASE: &str = "https://en.wikipedia.org/wiki/Portal:Current_events/";
pub const REST_HTML_BASE: &str = "https://en.wikipedia.org/api/rest_v1/page/html/";
pub const ATTRIBUTION: &str = "Source: Wikipedia Current Events Portal, CC BY-SA 4.0.";
pub const MAX_LOOKBACK_DAYS: i8 = 3;
const SUMMARY_BATCH_SIZE: usize = 6;
const ARTICLE_CONCURRENCY: usize = 8;
const MAX_LEAD_CHARS: usize = 2_400;

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
const CONTENT_SELECTORS: &[&str] = &[
    "div.current-events-content",
    "div.description",
    "div.current-events-main",
    "section",
    "body",
];
const DROP_ELEMENTS: &[&str] = &[
    "script", "style", "sup", "table", "figure", "img", "link", "meta", "noscript", "input",
    "button",
];
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

pub fn portal_title(date: Date) -> String {
    let month = MONTHS
        .get((date.month() as usize).saturating_sub(1))
        .copied()
        .unwrap_or("January");
    format!("{}_{}_{}", date.year(), month, date.day())
}

pub fn portal_url(date: Date) -> String {
    format!("{PORTAL_BASE}{}", portal_title(date))
}

pub fn rest_html_url(date: Date) -> String {
    format!(
        "{REST_HTML_BASE}Portal%3ACurrent_events%2F{}",
        portal_title(date)
    )
}

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
    let sections = extract_events(&html, &portal_url(date)).ok_or(WorldError::Empty(date))?;
    Ok(WorldBriefing {
        date,
        source_url: portal_url(date),
        overview: None,
        sections,
    })
}

/// Candidate pages begin with the previous calendar day, then walk backward.
pub fn candidate_days(issue_date: Date, max_days_back: i8) -> Vec<Date> {
    (1..=max_days_back.max(1))
        .map_while(|back| issue_date.checked_sub(jiff::Span::new().days(back)).ok())
        .collect()
}

pub async fn fetch_with_fallback(
    http: &reqwest::Client,
    issue_date: Date,
    max_days_back: i8,
) -> Result<WorldBriefing, WorldError> {
    let mut last = WorldError::Empty(issue_date);
    for day in candidate_days(issue_date, max_days_back) {
        match fetch(http, day).await {
            Ok(briefing) => return Ok(briefing),
            Err(error) => {
                tracing::debug!(%day, "world briefing not available for this day: {error}");
                last = error;
            }
        }
    }
    Err(last)
}

pub async fn fetch_optional(
    http: &reqwest::Client,
    date: Date,
    enabled: bool,
) -> Option<WorldBriefing> {
    if !enabled {
        return None;
    }
    match fetch_with_fallback(http, date, MAX_LOOKBACK_DAYS).await {
        Ok(briefing) => Some(briefing),
        Err(error) => {
            tracing::warn!(%date, "world briefing unavailable: {error}");
            None
        }
    }
}

fn dropped(element: &scraper::node::Element) -> bool {
    DROP_ELEMENTS.contains(&element.name())
        || element.attr("role") == Some("navigation")
        || element.attr("class").is_some_and(|class| {
            DROP_CLASSES
                .iter()
                .any(|drop| class.split_whitespace().any(|name| name == *drop))
        })
}

fn node_text(node: NodeRef<'_>, skip_lists: bool, out: &mut String) {
    match node.value() {
        Node::Text(text) => {
            out.push_str(&text);
            out.push(' ');
        }
        Node::Element(element) => {
            if dropped(&element) || (skip_lists && matches!(element.name(), "ul" | "ol")) {
                return;
            }
            for child in node.children() {
                node_text(child, skip_lists, out);
            }
        }
        _ => {}
    }
}

fn clean_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn element_text(element: ElementRef<'_>, skip_lists: bool) -> String {
    let mut out = String::new();
    node_text(*element, skip_lists, &mut out);
    clean_text(&out)
}

fn article_link(raw: &str, base: &Url) -> Option<String> {
    let mut url = if let Some(title) = raw.strip_prefix("./") {
        Url::parse(&format!("https://en.wikipedia.org/wiki/{title}")).ok()?
    } else {
        base.join(raw).ok()?
    };
    if url.scheme() != "https" || url.host_str() != Some("en.wikipedia.org") {
        return None;
    }
    let title = url.path().strip_prefix("/wiki/")?;
    if title.is_empty() || title.contains(':') {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn links_without_child_lists(element: ElementRef<'_>, base: &Url) -> Vec<String> {
    fn walk(node: NodeRef<'_>, base: &Url, links: &mut Vec<String>) {
        let Node::Element(element) = node.value() else {
            return;
        };
        if dropped(&element) || matches!(element.name(), "ul" | "ol") {
            return;
        }
        if element.name() == "a"
            && let Some(href) = element.attr("href")
            && let Some(url) = article_link(href, base)
            && !links.contains(&url)
        {
            links.push(url);
        }
        for child in node.children() {
            walk(child, base, links);
        }
    }
    let mut links = Vec::new();
    for child in element.children() {
        walk(child, base, &mut links);
    }
    links
}

fn parse_list(list: ElementRef<'_>, base: &Url, prefix: &str) -> Vec<WorldEvent> {
    list.children()
        .filter_map(ElementRef::wrap)
        .filter(|element| element.value().name() == "li" && !dropped(element.value()))
        .enumerate()
        .filter_map(|(index, li)| {
            let id = format!("{prefix}{}", index + 1);
            let source_text = element_text(li, true);
            let mut children = Vec::new();
            for child in li.children().filter_map(ElementRef::wrap) {
                if matches!(child.value().name(), "ul" | "ol") {
                    children.extend(parse_list(child, base, &format!("{id}-")));
                }
            }
            (!source_text.is_empty() || !children.is_empty()).then(|| WorldEvent {
                id,
                source_text,
                links: links_without_child_lists(li, base),
                children,
                summary: None,
            })
        })
        .collect()
}

fn category_title(element: ElementRef<'_>) -> Option<String> {
    if matches!(element.value().name(), "p" | "h2" | "h3" | "h4" | "dt") {
        let text = element_text(element, false);
        (!text.is_empty()).then_some(text)
    } else {
        None
    }
}

fn parse_container(
    container: ElementRef<'_>,
    base: &Url,
    section_offset: usize,
) -> Vec<WorldBriefingSection> {
    let mut current_title: Option<String> = None;
    let mut sections = Vec::new();
    for child in container.children().filter_map(ElementRef::wrap) {
        if dropped(child.value()) {
            continue;
        }
        if let Some(title) = category_title(child) {
            current_title = Some(title);
            continue;
        }
        if matches!(child.value().name(), "ul" | "ol") {
            let section_number = section_offset + sections.len() + 1;
            let events = parse_list(child, base, &format!("s{section_number}-e"));
            if !events.is_empty() {
                sections.push(WorldBriefingSection {
                    title: current_title
                        .take()
                        .unwrap_or_else(|| "Other events".into()),
                    events,
                });
            }
        }
    }
    sections
}

/// Parse all containers for the first selector that yields events.
pub fn extract_events(html: &str, source_url: &str) -> Option<Vec<WorldBriefingSection>> {
    let document = scraper::Html::parse_document(html);
    let base = Url::parse(source_url).ok()?;
    for selector in CONTENT_SELECTORS {
        let selector = scraper::Selector::parse(selector).ok()?;
        let mut sections = Vec::new();
        for container in document.select(&selector) {
            let parsed = parse_container(container, &base, sections.len());
            sections.extend(parsed);
        }
        if !sections.is_empty() {
            return Some(sections);
        }
    }
    None
}

#[derive(Clone)]
struct LeafInput {
    id: String,
    source_text: String,
    context: Vec<String>,
    links: Vec<String>,
}

fn collect_leaves(
    events: &[WorldEvent],
    ancestors: &[String],
    inherited_links: &[String],
    out: &mut Vec<LeafInput>,
) {
    for event in events {
        let mut context = ancestors.to_vec();
        let mut links = inherited_links.to_vec();
        for link in &event.links {
            if !links.contains(link) {
                links.push(link.clone());
            }
        }
        if event.children.is_empty() {
            out.push(LeafInput {
                id: event.id.clone(),
                source_text: event.source_text.clone(),
                context,
                links,
            });
        } else {
            if !event.source_text.is_empty() {
                context.push(event.source_text.clone());
            }
            collect_leaves(&event.children, &context, &links, out);
        }
    }
}

fn article_rest_url(article_url: &str) -> Option<Url> {
    let article = Url::parse(article_url).ok()?;
    let title = article.path().strip_prefix("/wiki/")?;
    Url::parse(&format!("{REST_HTML_BASE}{title}")).ok()
}

fn lead_text(html: &str) -> Option<String> {
    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("p").ok()?;
    let mut parts = Vec::new();
    for paragraph in document.select(&selector) {
        let text = element_text(paragraph, false);
        if text.len() >= 40 {
            parts.push(text);
        }
        if parts.len() == 3 {
            break;
        }
    }
    let text = parts.join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(MAX_LEAD_CHARS).collect())
    }
}

async fn fetch_leads(http: &reqwest::Client, links: &[String]) -> (HashMap<String, String>, usize) {
    let unique: HashSet<String> = links.iter().cloned().collect();
    let results = stream::iter(unique.into_iter().map(|link| {
        let http = http.clone();
        async move {
            let result = match article_rest_url(&link) {
                Some(url) => match http.get(url).send().await {
                    Ok(response) => match response.error_for_status() {
                        Ok(response) => {
                            response.text().await.ok().and_then(|html| lead_text(&html))
                        }
                        Err(_) => None,
                    },
                    Err(_) => None,
                },
                None => None,
            };
            (link, result)
        }
    }))
    .buffer_unordered(ARTICLE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let failures = results.iter().filter(|(_, lead)| lead.is_none()).count();
    let leads = results
        .into_iter()
        .filter_map(|(link, lead)| lead.map(|lead| (link, lead)))
        .collect();
    (leads, failures)
}

#[derive(Deserialize)]
struct SummaryResponse {
    summaries: HashMap<String, String>,
}

fn apply_summaries(events: &mut [WorldEvent], summaries: &HashMap<String, String>) {
    for event in events {
        if event.children.is_empty() {
            event.summary = summaries
                .get(&event.id)
                .map(|summary| clean_text(summary))
                .filter(|summary| !summary.is_empty());
        } else {
            apply_summaries(&mut event.children, summaries);
        }
    }
}

/// Enrich leaves after article editorial work has consumed its budget priority.
/// Every failure is reported as a warning while the source hierarchy survives.
pub async fn enrich(
    http: &reqwest::Client,
    briefing: &mut WorldBriefing,
    llm: Option<&LlmClient>,
) -> Vec<String> {
    let Some(llm) = llm else {
        return vec!["World Briefing enrichment skipped because the LLM is unavailable".into()];
    };
    let mut leaves = Vec::new();
    for section in &briefing.sections {
        collect_leaves(
            &section.events,
            std::slice::from_ref(&section.title),
            &[],
            &mut leaves,
        );
    }
    let all_links = leaves
        .iter()
        .flat_map(|leaf| leaf.links.clone())
        .collect::<Vec<_>>();
    let (leads, failed_links) = fetch_leads(http, &all_links).await;
    let mut warnings = Vec::new();
    if failed_links > 0 {
        warnings.push(format!(
            "World Briefing could not fetch {failed_links} linked Wikipedia article lead(s); source bullets were retained"
        ));
    }

    let mut summaries = HashMap::new();
    for batch in leaves.chunks(SUMMARY_BATCH_SIZE) {
        let items = batch
            .iter()
            .map(|leaf| serde_json::json!({
                "id": leaf.id,
                "ancestor_labels": leaf.context,
                "source_bullet": leaf.source_text,
                "wikipedia_leads": leaf.links.iter().filter_map(|link| leads.get(link)).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>();
        let prompt = format!(
            "Summarize each World Briefing event in two sentences and approximately 35-60 words. Use only the source bullet and Wikipedia leads supplied. Return JSON exactly as {{\"summaries\":{{\"event-id\":\"summary\"}}}}; keys must be the supplied stable IDs. Events:\n{}",
            serde_json::to_string(&items).unwrap_or_default()
        );
        match llm.complete_json::<SummaryResponse>(&prompt, 0.2).await {
            Ok(response) => {
                for leaf in batch {
                    if let Some(summary) = response.summaries.get(&leaf.id) {
                        summaries.insert(leaf.id.clone(), summary.clone());
                    }
                }
            }
            Err(error) => {
                warnings.push(format!("World Briefing summary batch failed: {error}"));
                if matches!(error, LlmError::BudgetExceeded { .. }) {
                    break;
                }
            }
        }
    }
    for section in &mut briefing.sections {
        apply_summaries(&mut section.events, &summaries);
    }

    if !summaries.is_empty() {
        let ordered = leaves
            .iter()
            .filter_map(|leaf| {
                summaries
                    .get(&leaf.id)
                    .map(|summary| format!("{}: {summary}", leaf.id))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Write a neutral 150-220 word overview of this completed news day in 2-3 paragraphs. Ground it only in these event summaries and return plain text with blank lines between paragraphs:\n{ordered}"
        );
        match llm.complete_text(&prompt, 0.2).await {
            Ok(overview) if !overview.trim().is_empty() => {
                briefing.overview = Some(overview.trim().to_string())
            }
            Ok(_) => warnings
                .push("World Briefing overview was empty; source bullets were retained".into()),
            Err(error) => warnings.push(format!("World Briefing overview failed: {error}")),
        }
    }
    warnings
}

fn render_events(events: &[WorldEvent], out: &mut String) {
    out.push_str("<ul>");
    for event in events {
        out.push_str("<li>");
        out.push_str(&text_escape(&event.source_text));
        if let Some(summary) = &event.summary {
            out.push_str("<p class=\"world-summary\">");
            out.push_str(&text_escape(summary));
            out.push_str("</p>");
        }
        if !event.children.is_empty() {
            render_events(&event.children, out);
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
}

pub fn render_xhtml(briefing: &WorldBriefing) -> String {
    let mut out = String::new();
    if let Some(overview) = &briefing.overview {
        for paragraph in overview
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            out.push_str("<p class=\"world-overview\">");
            out.push_str(&text_escape(paragraph));
            out.push_str("</p>");
        }
    }
    for section in &briefing.sections {
        out.push_str("<h2>");
        out.push_str(&text_escape(&section.title));
        out.push_str("</h2>");
        render_events(&section.events, &mut out);
    }
    out.push_str("<p class=\"attribution\">");
    out.push_str(&text_escape(ATTRIBUTION));
    out.push_str(" <a href=\"");
    out.push_str(&text_escape(&briefing.source_url));
    out.push_str("\">");
    out.push_str(&text_escape(&briefing.source_url));
    out.push_str("</a></p>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/wikipedia_current_events.html"),
        )
        .expect("fixture")
    }

    #[test]
    fn parses_categories_hierarchy_and_article_links() {
        let sections =
            extract_events(&fixture(), &portal_url("2026-08-15".parse().unwrap())).unwrap();
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].title, "Armed conflicts and attacks");
        assert_eq!(sections[0].events.len(), 2);
        assert_eq!(sections[0].events[0].children.len(), 1);
        assert_eq!(sections[0].events[0].id, "s1-e1");
        assert_eq!(sections[0].events[0].children[0].id, "s1-e1-1");
        assert!(
            sections[0].events[0]
                .links
                .iter()
                .any(|url| url.ends_with("/Border_dispute"))
        );
        assert!(
            sections[0].events[0]
                .links
                .iter()
                .any(|url| url.ends_with("/United_Nations"))
        );
        assert!(
            sections[1].events[0]
                .links
                .iter()
                .any(|url| url.ends_with("/Boston"))
        );
        assert!(sections.iter().all(|section| {
            section
                .events
                .iter()
                .all(|event| !event.source_text.contains("[1]"))
        }));
    }

    #[test]
    fn first_viable_selector_keeps_every_container() {
        let html = r#"<div class="description"><p><b>One</b></p><ul><li>A</li></ul></div>
          <div class="description"><p><b>Two</b></p><ul><li>B</li></ul></div>"#;
        let sections = extract_events(
            html,
            "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_14",
        )
        .unwrap();
        assert_eq!(
            sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["One", "Two"]
        );
        assert_eq!(sections[1].events[0].id, "s2-e1");
    }

    #[test]
    fn filters_external_special_fragment_and_edit_links() {
        let html = r#"<div class="description"><p><b>News</b></p><ul><li>
          <a href="./Valid_article#History">valid</a><a href="https://example.com/x">external</a>
          <a href="./Special:Random">special</a><a href="/w/index.php?title=X">edit</a></li></ul></div>"#;
        let sections = extract_events(
            html,
            "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_14",
        )
        .unwrap();
        assert_eq!(
            sections[0].events[0].links,
            ["https://en.wikipedia.org/wiki/Valid_article"]
        );
    }

    #[test]
    fn candidates_start_yesterday_across_boundaries() {
        assert_eq!(
            candidate_days("2026-01-01".parse().unwrap(), 3),
            [
                "2025-12-31".parse().unwrap(),
                "2025-12-30".parse().unwrap(),
                "2025-12-29".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn missing_events_yield_none() {
        assert!(
            extract_events(
                "<html><body><p>Nothing</p></body></html>",
                "https://en.wikipedia.org/wiki/X"
            )
            .is_none()
        );
    }

    #[test]
    fn rendering_preserves_hierarchy_summaries_and_attribution() {
        let mut briefing = WorldBriefing {
            date: "2026-08-14".parse().unwrap(),
            source_url: portal_url("2026-08-14".parse().unwrap()),
            overview: Some("First paragraph.\n\nSecond paragraph.".into()),
            sections: extract_events(&fixture(), &portal_url("2026-08-14".parse().unwrap()))
                .unwrap(),
        };
        briefing.sections[0].events[0].children[0].summary = Some("Grounded detail.".into());
        let xhtml = render_xhtml(&briefing);
        assert!(xhtml.contains("world-overview"));
        assert!(xhtml.contains("world-summary"));
        assert!(xhtml.contains("Grounded detail."));
        assert!(xhtml.contains("CC BY-SA 4.0"));
    }

    fn two_leaf_briefing() -> WorldBriefing {
        WorldBriefing {
            date: "2026-08-14".parse().unwrap(),
            source_url: portal_url("2026-08-14".parse().unwrap()),
            overview: None,
            sections: vec![WorldBriefingSection {
                title: "News".into(),
                events: vec![
                    WorldEvent {
                        id: "s1-e1".into(),
                        source_text: "First raw bullet.".into(),
                        links: vec![],
                        children: vec![],
                        summary: None,
                    },
                    WorldEvent {
                        id: "s1-e2".into(),
                        source_text: "Second raw bullet.".into(),
                        links: vec![],
                        children: vec![],
                        summary: None,
                    },
                ],
            }],
        }
    }

    fn mock_client(
        backend: std::sync::Arc<crate::curate::llm::MockBackend>,
        limit: f64,
    ) -> LlmClient {
        let config = crate::config::DeepseekConfig::default();
        LlmClient::with_backend(
            "mock",
            "World Briefing test".into(),
            crate::curate::llm::UsageMeter::new(&config, limit),
            backend,
        )
    }

    #[tokio::test]
    async fn enrichment_is_keyed_by_id_and_overview_uses_every_summary() {
        let backend = std::sync::Arc::new(crate::curate::llm::MockBackend::new());
        backend.push(
            r#"{"summaries":{"s1-e2":"Summary for the second event.","s1-e1":"Summary for the first event."}}"#,
            crate::types::TokenUsage::default(),
        );
        backend.push(
            "A complete overview paragraph.\n\nA second overview paragraph.",
            crate::types::TokenUsage::default(),
        );
        let llm = mock_client(backend.clone(), 1.0);
        let mut briefing = two_leaf_briefing();
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let warnings = enrich(&http, &mut briefing, Some(&llm)).await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            briefing.sections[0].events[0].summary.as_deref(),
            Some("Summary for the first event.")
        );
        assert_eq!(
            briefing.sections[0].events[1].summary.as_deref(),
            Some("Summary for the second event.")
        );
        assert!(
            briefing
                .overview
                .as_deref()
                .unwrap()
                .contains("complete overview")
        );
        let prompts = backend.prompts();
        assert!(prompts[1].user.contains("Summary for the first event."));
        assert!(prompts[1].user.contains("Summary for the second event."));
    }

    #[tokio::test]
    async fn malformed_or_unavailable_llm_preserves_every_raw_bullet() {
        let backend = std::sync::Arc::new(crate::curate::llm::MockBackend::new());
        backend.push("not json", crate::types::TokenUsage::default());
        let llm = mock_client(backend, 1.0);
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let mut malformed = two_leaf_briefing();
        let warnings = enrich(&http, &mut malformed, Some(&llm)).await;
        assert!(!warnings.is_empty());
        assert_eq!(
            malformed.sections[0]
                .events
                .iter()
                .map(|event| event.source_text.as_str())
                .collect::<Vec<_>>(),
            ["First raw bullet.", "Second raw bullet."]
        );
        assert!(
            malformed.sections[0]
                .events
                .iter()
                .all(|event| event.summary.is_none())
        );

        let mut unavailable = two_leaf_briefing();
        let warnings = enrich(&http, &mut unavailable, None).await;
        assert_eq!(unavailable, two_leaf_briefing());
        assert_eq!(warnings.len(), 1);
    }

    #[tokio::test]
    async fn exhausted_budget_makes_no_backend_call_and_keeps_bullets() {
        let backend = std::sync::Arc::new(crate::curate::llm::MockBackend::new());
        let llm = mock_client(backend.clone(), 0.01);
        llm.meter.preload_cost(0.01);
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let mut briefing = two_leaf_briefing();
        let warnings = enrich(&http, &mut briefing, Some(&llm)).await;
        assert_eq!(backend.calls(), 0);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("cost ceiling"))
        );
        assert_eq!(briefing, two_leaf_briefing());
    }

    #[test]
    fn leaf_context_inherits_and_deduplicates_parent_article_links() {
        let events = vec![WorldEvent {
            id: "s1-e1".into(),
            source_text: "Parent topic".into(),
            links: vec!["https://en.wikipedia.org/wiki/Parent".into()],
            summary: None,
            children: vec![WorldEvent {
                id: "s1-e1-1".into(),
                source_text: "Leaf event".into(),
                links: vec![
                    "https://en.wikipedia.org/wiki/Parent".into(),
                    "https://en.wikipedia.org/wiki/Leaf".into(),
                ],
                children: vec![],
                summary: None,
            }],
        }];
        let mut leaves = Vec::new();
        collect_leaves(&events, &["News".into()], &[], &mut leaves);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].context, ["News", "Parent topic"]);
        assert_eq!(leaves[0].links.len(), 2);
        assert!(leaves[0].links.iter().any(|link| link.ends_with("/Parent")));
        assert!(leaves[0].links.iter().any(|link| link.ends_with("/Leaf")));
    }
}
