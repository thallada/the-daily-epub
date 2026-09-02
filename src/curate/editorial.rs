//! Editor-first summaries and The Brief, with per-call bulk fallback (§14).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use futures::{StreamExt, stream};
use serde::Deserialize;

use super::llm::{LlmClient, LlmError, Llms};
use super::{escape_html, prompt_text, text_to_paragraphs, truncate_tokens, truncate_words};
use crate::config::{EditorialConfig, SummaryModel};
use crate::types::{ArticleId, Editorial, Lineup, Pick};

pub const FALLBACK_SUMMARY_WORDS: usize = 45;

pub const SUMMARY_INSTRUCTIONS: &str = "\
TASK: write the newspaper abstract for one article in today's issue.

Two or three sentences, 40–70 words, present tense, third person. It runs under \
the headline in the \"In This Issue\" page, so the reader decides from it alone \
whether to open the piece.

DO
- Say what the article actually argues, reports or builds — the specific claim, \
number, method or story, not the topic.
- Add the one detail that makes it worth his time: the surprising result, the \
scale, the person involved, the unusual method.
- Match the piece's register: a technical post-mortem gets a technical abstract, \
an essay gets an essayistic one.
- Stay strictly inside the supplied text.

DO NOT
- Tease (\"you won't believe what happens next\"), moralize, or address the \
reader as \"you\".
- Open with \"This article…\", \"The author…\", \"In this post…\", or repeat the \
headline's words.
- Invent facts, names, numbers or conclusions that are not in the text. If the \
text is a truncated excerpt, summarize only what is there and say it is an \
excerpt.
- Recommend, rate or editorialize — that is the front page's job.

Return JSON exactly: {\"summary\": \"<two or three sentences>\"}";

/// The Brief instructions (§14.2).
pub const BRIEF_INSTRUCTIONS: &str = r#"TASK: write "The Brief" for today's issue — the note at the top of the paper.

120-200 words, one or two paragraphs. It must earn its place: if a reader skipped
it, what would he miss? Name at least three of today's picks by title and say the
specific thing that makes each worth his time (the result, the argument, the scale,
the person). If there is a thread connecting several pieces, say it in one sentence;
if there is not, do not invent one. If the issue is short, say why in one clause.

Do not: welcome the reader, describe the weather, summarize every section, use
"delve", "dive", "explore", "a mix of", "something for everyone", or any sentence
that could introduce any other issue. No headings. No bullet points.

Return JSON exactly: {"brief": "<the text, plain prose>"}"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BriefResponse {
    #[serde(default)]
    pub brief: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    summary: String,
}

fn summary_prompt(title: &str, body_html: &str, input_tokens: usize) -> String {
    let body = truncate_tokens(&prompt_text(body_html), input_tokens);
    let mut prompt = String::with_capacity(body.len() + SUMMARY_INSTRUCTIONS.len() + 256);
    prompt.push_str(SUMMARY_INSTRUCTIONS);
    let _ = write!(
        prompt,
        "\n\nHEADLINE: {}\n\nARTICLE TEXT{}:\n{}\n",
        title.trim(),
        if body.ends_with('…') {
            " (truncated for length)"
        } else {
            ""
        },
        if body.is_empty() {
            "(no body text was extracted; summarize from the headline alone and say the full text was unavailable)"
        } else {
            &body
        }
    );
    prompt
}

pub async fn summarize_article(
    llm: &LlmClient,
    title: &str,
    body_html: &str,
    input_tokens: usize,
    temperature: f32,
) -> Result<String, LlmError> {
    let prompt = summary_prompt(title, body_html, input_tokens);
    let response: SummaryResponse = llm.complete_json(&prompt, temperature).await?;
    let summary = response.summary.trim().to_string();
    if summary.is_empty() {
        return Err(LlmError::empty_response(llm.provider()));
    }
    Ok(summary)
}

/// `(primary, fallback)` for the summaries per `editorial.summary_model` (§14.1).
fn summary_clients(llms: &Llms, model: SummaryModel) -> (Option<&LlmClient>, Option<&LlmClient>) {
    match model {
        SummaryModel::Bulk => (llms.bulk.as_ref(), None),
        SummaryModel::Editor => {
            let primary = llms.editor_or_bulk();
            let fallback = primary.and_then(|client| {
                llms.bulk
                    .as_ref()
                    .filter(|bulk| bulk.provider != client.provider)
            });
            (primary, fallback)
        }
    }
}

async fn summarize_pick(
    pick: &Pick,
    primary: Option<&LlmClient>,
    fallback: Option<&LlmClient>,
    config: &EditorialConfig,
    temperature: f32,
) -> Option<String> {
    let primary = primary?;
    match summarize_article(
        primary,
        &pick.article.title,
        &pick.article.content_html,
        config.summary_input_tokens,
        temperature,
    )
    .await
    {
        Ok(summary) => Some(summary),
        Err(error) => {
            let Some(fallback) = fallback else {
                tracing::warn!(article_id = pick.article.id, %error, "summary failed; using excerpt");
                return None;
            };
            tracing::warn!(article_id = pick.article.id, %error, "editor summary failed; retrying on bulk");
            summarize_article(
                fallback,
                &pick.article.title,
                &pick.article.content_html,
                config.summary_input_tokens,
                temperature,
            )
            .await
            .map_err(|fallback_error| {
                tracing::warn!(article_id = pick.article.id, %fallback_error, "bulk summary failed; using excerpt");
            })
            .ok()
        }
    }
}

pub async fn summarize_all(
    llms: &Llms,
    lineup: &Lineup,
    config: &EditorialConfig,
    temperature: f32,
) -> BTreeMap<ArticleId, String> {
    let (primary, fallback) = summary_clients(llms, config.summary_model);
    // The summary provider's own `max_concurrent_requests` bounds the fan-out.
    let concurrency = primary
        .map(|client| client.max_concurrent_requests)
        .unwrap_or(1)
        .max(1);
    stream::iter(lineup.picks.iter())
        .map(|pick| async move {
            let summary = summarize_pick(pick, primary, fallback, config, temperature).await;
            (pick.article.id, summary)
        })
        .buffer_unordered(concurrency)
        .filter_map(|(id, summary)| async move { summary.map(|summary| (id, summary)) })
        .collect()
        .await
}

pub fn build_brief_prompt(lineup: &Lineup, summaries: &BTreeMap<ArticleId, String>) -> String {
    let mut prompt = String::with_capacity(4096);
    prompt.push_str(BRIEF_INSTRUCTIONS);
    let _ = write!(
        prompt,
        "\n\nISSUE: {} · {} articles\n\nLINEUP\n",
        lineup.date,
        lineup.picks.len()
    );
    for section in &lineup.section_order {
        let _ = writeln!(prompt, "\n## {section}");
        for pick in lineup.section_picks(section) {
            let quality = pick
                .llm
                .as_ref()
                .map(|assessment| format!("{:.1}", assessment.quality))
                .unwrap_or_else(|| "unassessed".into());
            let fit = pick
                .llm
                .as_ref()
                .map(|assessment| format!("{:.1}", assessment.fit))
                .unwrap_or_else(|| "unassessed".into());
            let summary = summaries
                .get(&pick.article.id)
                .cloned()
                .unwrap_or_else(|| excerpt_summary(pick));
            let _ = writeln!(
                prompt,
                "- {}\n  feed: {}\n  why: {}\n  quality: {}\n  fit: {}\n  summary: {}",
                pick.article.title.trim(),
                pick.article.feed_title.trim(),
                pick.why.as_deref().unwrap_or("not supplied"),
                quality,
                fit,
                summary
            );
        }
    }
    prompt
}

pub async fn brief(
    llms: &Llms,
    lineup: &Lineup,
    summaries: &BTreeMap<ArticleId, String>,
    temperature: f32,
) -> Result<String, LlmError> {
    let prompt = build_brief_prompt(lineup, summaries);
    let Some(primary) = llms.editor_or_bulk() else {
        return Err(LlmError::api("editorial", "no provider configured"));
    };
    let response = match primary
        .complete_json::<BriefResponse>(&prompt, temperature)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let Some(fallback) = llms
                .bulk
                .as_ref()
                .filter(|bulk| bulk.provider != primary.provider)
            else {
                return Err(error);
            };
            tracing::warn!(%error, "brief failed on editor; retrying on bulk");
            fallback
                .complete_json::<BriefResponse>(&prompt, temperature)
                .await?
        }
    };
    let brief = response.brief.trim().to_string();
    if brief.is_empty() {
        return Err(LlmError::empty_response(primary.provider()));
    }
    Ok(brief)
}

pub fn excerpt_summary(pick: &Pick) -> String {
    let text = truncate_words(
        &prompt_text(&pick.article.content_html),
        FALLBACK_SUMMARY_WORDS,
    );
    if text.is_empty() {
        format!(
            "From {}. (No preview text was available; open the article to read it.)",
            if pick.article.feed_title.is_empty() {
                "an unknown feed"
            } else {
                pick.article.feed_title.trim()
            }
        )
    } else {
        text
    }
}

pub fn fallback_front_page_html(lineup: &Lineup) -> String {
    let minutes: i64 = lineup
        .picks
        .iter()
        .map(|pick| pick.article.reading_minutes())
        .sum();
    let mut text = format!(
        "Today's issue collects {} articles across {} sections — about {} minutes of reading. Editorial notes are unavailable for this issue, so the lineup speaks for itself.",
        lineup.picks.len(),
        lineup.section_order.len(),
        minutes
    );
    if let Some(lead) = lineup.lead() {
        let _ = write!(
            text,
            "\n\nLeading today: “{}” ({}).",
            lead.article.title.trim(),
            lead.article.feed_title.trim()
        );
    }
    text_to_paragraphs(&text)
}

pub fn fallback_editorial(lineup: &Lineup) -> Editorial {
    Editorial {
        front_page_html: fallback_front_page_html(lineup),
        summaries: lineup
            .picks
            .iter()
            .map(|pick| (pick.article.id, excerpt_summary(pick)))
            .collect(),
    }
}

/// Wall-clock milliseconds of the two editorial calls, for the run report's
/// `summaries` and `brief` stage timings (§15.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorialTimings {
    pub summaries_ms: i64,
    pub brief_ms: i64,
}

pub async fn run(
    llms: &Llms,
    lineup: &Lineup,
    config: &EditorialConfig,
    temperature: f32,
) -> Editorial {
    run_timed(llms, lineup, config, temperature).await.0
}

/// [`run`], also reporting how long the summaries and the brief took.
pub async fn run_timed(
    llms: &Llms,
    lineup: &Lineup,
    config: &EditorialConfig,
    temperature: f32,
) -> (Editorial, EditorialTimings) {
    if lineup.picks.is_empty() {
        return (fallback_editorial(lineup), EditorialTimings::default());
    }
    let started = std::time::Instant::now();
    let mut summaries = summarize_all(llms, lineup, config, temperature).await;
    for pick in &lineup.picks {
        summaries
            .entry(pick.article.id)
            .or_insert_with(|| excerpt_summary(pick));
    }
    let summaries_ms = started.elapsed().as_millis() as i64;
    let started = std::time::Instant::now();
    let front_page_html = match brief(llms, lineup, &summaries, temperature).await {
        Ok(text) => text_to_paragraphs(&text),
        Err(error) => {
            tracing::warn!(%error, "brief failed; using fallback front page");
            fallback_front_page_html(lineup)
        }
    };
    let brief_ms = started.elapsed().as_millis() as i64;
    (
        Editorial {
            front_page_html,
            summaries,
        },
        EditorialTimings {
            summaries_ms,
            brief_ms,
        },
    )
}

pub fn summary_to_html(summary: &str) -> String {
    format!("<p>{}</p>", escape_html(summary.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;
    use crate::curate::llm::{ChatBackend, MockBackend, PriceTable, UsageMeter};
    use crate::curate::prefilter::tests::article;
    use crate::types::TokenUsage;
    use std::sync::Arc;

    const BRIEF_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/claude_brief.json"
    ));

    fn pick(id: i64, title: &str, section: &str, is_lead: bool) -> Pick {
        let mut a = article(id, title, 900);
        a.content_html = format!("<p>{title} opens with a specific, concrete claim.</p>");
        Pick {
            article: a,
            section: section.into(),
            position: 1,
            is_lead,
            why: Some(format!("the {title} piece you'd argue with")),
            summary: None,
            llm: None,
            discussion: None,
        }
    }

    fn lineup() -> Lineup {
        Lineup {
            date: "2026-08-15".parse().expect("date"),
            picks: vec![
                pick(1, "Migrating 40TB off Postgres", "Top Stories", true),
                pick(2, "The MBTA slow-zone dataset", "Boston & Local", false),
            ],
            section_order: vec!["Top Stories".into(), "Boston & Local".into()],
        }
    }

    fn mock(provider: &'static str, backend: Arc<MockBackend>, limit: f64) -> LlmClient {
        let prices = if provider == "anthropic" {
            PriceTable::from(&ProviderConfig::anthropic())
        } else {
            PriceTable::from(&ProviderConfig::deepseek())
        };
        LlmClient::with_backend_options(
            provider,
            "model",
            "SYSTEM".into(),
            None,
            UsageMeter::with_prices(prices, limit),
            backend as Arc<dyn ChatBackend>,
        )
    }

    fn bulk_only(backend: Arc<MockBackend>, limit: f64) -> Llms {
        Llms {
            bulk: Some(mock("deepseek", backend, limit)),
            editor: None,
        }
    }

    fn editor_and_bulk(editor: Arc<MockBackend>, bulk: Arc<MockBackend>) -> Llms {
        Llms {
            bulk: Some(mock("deepseek", bulk, 2.0)),
            editor: Some(mock("anthropic", editor, 3.0)),
        }
    }

    fn config() -> EditorialConfig {
        EditorialConfig::default()
    }

    #[tokio::test]
    async fn summary_prompt_carries_headline_and_truncated_body() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"summary": "A team moves 40TB of relational data off Postgres and documents every rollback."}"#,
            TokenUsage::default(),
        );
        let llm = mock("deepseek", Arc::clone(&backend), 2.0);
        let body = format!("<p>{}</p>", "word ".repeat(20_000));
        let summary = summarize_article(&llm, "Migrating 40TB", &body, 3_000, 0.8)
            .await
            .expect("summary");
        assert!(summary.starts_with("A team moves 40TB"));

        let prompt = &backend.prompts()[0].user;
        assert!(prompt.starts_with(SUMMARY_INSTRUCTIONS));
        assert!(prompt.contains("HEADLINE: Migrating 40TB"));
        assert!(prompt.contains("(truncated for length)"));
        // 3k tokens ≈ 12k characters of body, not the full 100k.
        assert!(prompt.len() < 16_000, "prompt was {} bytes", prompt.len());
    }

    #[tokio::test]
    async fn the_brief_is_parsed_and_rendered() {
        let backend = Arc::new(MockBackend::new());
        backend.push(BRIEF_FIXTURE, TokenUsage::default());
        let llms = bulk_only(Arc::clone(&backend), 2.0);
        let lineup = lineup();
        let summaries = BTreeMap::from([
            (1, "A migration story with numbers.".to_string()),
            (2, "Transit data, charted.".to_string()),
        ]);

        let text = brief(&llms, &lineup, &summaries, 0.8).await.expect("brief");
        assert!(text.split_whitespace().count() > 100);
        assert!(text.contains("Migrating 40TB off Postgres"));

        let prompt = &backend.prompts()[0].user;
        assert!(prompt.starts_with(BRIEF_INSTRUCTIONS));
        assert!(prompt.contains("## Top Stories"));
        assert!(prompt.contains("## Boston & Local"));
        assert!(prompt.contains("- Migrating 40TB off Postgres"));
        assert!(prompt.contains("why: the Migrating 40TB off Postgres piece you'd argue with"));
        assert!(prompt.contains("summary: A migration story with numbers."));
        assert!(prompt.contains("quality: unassessed"));
        assert!(prompt.contains("fit: unassessed"));
        assert!(prompt.contains("2026-08-15"));
        assert!(
            !prompt.contains("section_intros"),
            "section intros are gone"
        );
    }

    #[tokio::test]
    async fn full_stage_c_produces_summaries_and_the_brief() {
        let backend = Arc::new(MockBackend::new());
        backend.push(r#"{"summary": "First abstract."}"#, TokenUsage::default());
        backend.push(r#"{"summary": "Second abstract."}"#, TokenUsage::default());
        backend.push(BRIEF_FIXTURE, TokenUsage::default());
        let llms = bulk_only(Arc::clone(&backend), 2.0);

        let editorial = run(&llms, &lineup(), &config(), 0.8).await;
        assert_eq!(backend.calls(), 3, "one call per article plus the brief");
        assert_eq!(editorial.summaries.len(), 2);
        assert_eq!(editorial.summaries[&1], "First abstract.");
        assert!(editorial.front_page_html.starts_with("<p>"));
        assert!(editorial.front_page_html.contains("</p>"));
        assert!(
            editorial
                .front_page_html
                .contains("Migrating 40TB off Postgres")
        );
        assert!(!editorial.front_page_html.contains("<script"));
    }

    #[tokio::test]
    async fn summaries_run_on_the_editor_and_fall_back_per_article() {
        let editor = Arc::new(MockBackend::new());
        editor.push(
            r#"{"summary": "Opus wrote this one."}"#,
            TokenUsage::default(),
        );
        editor.push_llm_error(LlmError::refusal("anthropic"));
        editor.push(BRIEF_FIXTURE, TokenUsage::default());
        let bulk = Arc::new(MockBackend::new());
        bulk.push(
            r#"{"summary": "DeepSeek covered the refusal."}"#,
            TokenUsage::default(),
        );
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));

        let editorial = run(&llms, &lineup(), &config(), 0.8).await;
        assert_eq!(
            editor.calls(),
            3,
            "two summaries and the brief on the editor"
        );
        assert_eq!(bulk.calls(), 1, "only the refused summary went to bulk");
        assert_eq!(editorial.summaries[&1], "Opus wrote this one.");
        assert_eq!(editorial.summaries[&2], "DeepSeek covered the refusal.");
        assert_eq!(
            editor.prompts()[1].user,
            bulk.prompts()[0].user,
            "the bulk client gets the identical summary prompt"
        );
        assert!(
            editorial
                .front_page_html
                .contains("Migrating 40TB off Postgres")
        );
    }

    #[tokio::test]
    async fn the_brief_falls_back_to_bulk_with_the_same_prompt() {
        let editor = Arc::new(MockBackend::new());
        editor.push_error("500 opus is down");
        let bulk = Arc::new(MockBackend::new());
        bulk.push(BRIEF_FIXTURE, TokenUsage::default());
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));

        let text = brief(&llms, &lineup(), &BTreeMap::new(), 0.8)
            .await
            .expect("bulk brief");
        assert!(text.contains("Migrating 40TB off Postgres"));
        assert_eq!(editor.prompts()[0].user, bulk.prompts()[0].user);
    }

    #[tokio::test]
    async fn summary_model_bulk_skips_the_editor_for_summaries() {
        let editor = Arc::new(MockBackend::new());
        editor.push(BRIEF_FIXTURE, TokenUsage::default());
        let bulk = Arc::new(MockBackend::new());
        bulk.push(r#"{"summary": "First abstract."}"#, TokenUsage::default());
        bulk.push(r#"{"summary": "Second abstract."}"#, TokenUsage::default());
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));
        let config = EditorialConfig {
            summary_model: SummaryModel::Bulk,
            ..EditorialConfig::default()
        };

        let editorial = run(&llms, &lineup(), &config, 0.8).await;
        assert_eq!(bulk.calls(), 2);
        assert_eq!(editor.calls(), 1, "the brief still runs on the editor");
        assert_eq!(editorial.summaries[&2], "Second abstract.");
    }

    #[tokio::test]
    async fn budget_exhaustion_degrades_to_excerpts() {
        let backend = Arc::new(MockBackend::new());
        // The first summary alone blows a $0.05 ceiling.
        backend.push(
            r#"{"summary": "The one summary we could afford."}"#,
            TokenUsage {
                input_tokens: 1_000_000,
                cached_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
            },
        );
        let llms = bulk_only(Arc::clone(&backend), 0.05);

        let editorial = run(&llms, &lineup(), &config(), 0.8).await;
        assert_eq!(backend.calls(), 1, "no further calls after the ceiling");
        assert!(llms.bulk.as_ref().expect("bulk").meter.budget_exceeded());
        assert_eq!(
            editorial.summaries.len(),
            2,
            "every pick still has a summary"
        );
        assert_eq!(editorial.summaries[&1], "The one summary we could afford.");
        assert!(
            editorial.summaries[&2].contains("opens with a specific"),
            "second summary fell back to the excerpt: {}",
            editorial.summaries[&2]
        );
        // The front page degraded to the plain version.
        assert!(editorial.front_page_html.contains("2 articles"));
    }

    #[tokio::test]
    async fn a_failed_summary_call_is_not_fatal() {
        let backend = Arc::new(MockBackend::new());
        backend.push_error("400 bad request");
        backend.push(r#"{"summary": "Second abstract."}"#, TokenUsage::default());
        backend.push_error("500 brief exploded");
        let llms = bulk_only(Arc::clone(&backend), 2.0);

        let editorial = run(&llms, &lineup(), &config(), 0.8).await;
        assert_eq!(editorial.summaries.len(), 2);
        assert!(editorial.summaries[&1].contains("opens with a specific"));
        assert_eq!(editorial.summaries[&2], "Second abstract.");
        assert!(editorial.front_page_html.contains("Leading today"));
    }

    #[tokio::test]
    async fn no_provider_means_the_fallback_editorial() {
        let editorial = run(&Llms::default(), &lineup(), &config(), 0.8).await;
        assert_eq!(editorial.summaries.len(), 2);
        assert!(editorial.front_page_html.contains("2 articles"));
    }

    #[test]
    fn fallback_editorial_covers_every_pick() {
        let lineup = lineup();
        let editorial = fallback_editorial(&lineup);
        assert_eq!(editorial.summaries.len(), lineup.picks.len());
        assert!(editorial.front_page_html.contains("2 articles"));
        assert!(editorial.front_page_html.contains("2 sections"));
        assert!(editorial.front_page_html.starts_with("<p>"));

        // An empty lineup is still a valid editorial.
        let empty = Lineup {
            date: "2026-08-15".parse().expect("date"),
            picks: vec![],
            section_order: vec![],
        };
        let editorial = fallback_editorial(&empty);
        assert!(editorial.summaries.is_empty());
        assert!(editorial.front_page_html.contains("0 articles"));
    }

    #[test]
    fn excerpt_summary_handles_empty_bodies() {
        let mut p = pick(9, "No body here", "Top Stories", false);
        p.article.content_html = String::new();
        assert!(excerpt_summary(&p).contains("No preview text"));
        assert_eq!(summary_to_html("a <b> c"), "<p>a &lt;b&gt; c</p>");
    }
}
