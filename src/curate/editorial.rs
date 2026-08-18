//! Stage C — summaries, section intros and the front page (spec §3.6).
//!
//! Voice: warm, literate, a little playful; never fabricates facts that are not
//! present in the summaries.
//!
//! Everything here is best-effort. If the cost ceiling trips mid-way (§3.6) or a
//! call fails, the affected article silently falls back to its own opening words
//! and the run continues — an issue with plain excerpts is far better than no
//! issue at all.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::llm::{LlmClient, LlmError};
use super::{escape_html, prompt_text, text_to_paragraphs, truncate_tokens, truncate_words};
use crate::types::{ArticleId, Editorial, Lineup, Pick};

/// Article text is truncated to roughly this many tokens per summary call (§3.6).
pub const SUMMARY_INPUT_TOKEN_BUDGET: usize = 5000;
/// Target length of the "From the Editor" front page, in words (§3.6).
pub const FRONT_PAGE_WORDS: (usize, usize) = (250, 400);
/// Words of body text used when a summary has to fall back to the excerpt.
pub const FALLBACK_SUMMARY_WORDS: usize = 45;

// ---------------------------------------------------------------------------
// Prompts (reusable instructions here; per-call material in the user message)
// ---------------------------------------------------------------------------

/// Per-article summary instructions (§3.6 stage C).
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

/// Front-page + section-intro instructions (§3.6 stage C).
pub const FRONT_PAGE_INSTRUCTIONS: &str = "\
TASK: write the front page of today's issue of The Daily EPUB.

You are given the whole lineup: sections, headlines, sources and the abstract \
written for each article. Everything you write must come from those abstracts — \
you have not read the articles themselves, and inventing a fact would be worse \
than saying less.

Produce two things.

1. \"from_the_editor\" — 250 to 400 words of prose addressed to the paper's one \
reader. Find the two or three threads that actually run through today's lineup \
(a shared question, an argument between two pieces, an accidental theme) and use \
them to guide the read: what to start with over coffee, what to save for the \
commute, what rewards patience. Name the lead story and say why it leads. It is \
fine — good, even — to note when a day is quiet or lopsided. Voice: warm, \
literate, lightly playful, never breathless; a real editor writing to someone \
whose taste he knows. No bullet lists, no headings, no emoji, 2–4 paragraphs \
separated by a blank line.

2. \"section_intros\" — for EACH section name given below, two or three \
sentences (35–60 words) introducing what is in it today. Concrete, specific to \
these articles, no filler like \"a variety of interesting stories\". Use the \
section names exactly as spelled in the lineup.

Return JSON exactly:
{\"from_the_editor\": \"<paragraphs separated by \\n\\n>\", \
\"section_intros\": {\"<section name>\": \"<2-3 sentences>\"}}";

/// The single front-page call's JSON response (§3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontPageResponse {
    /// "From the Editor", 250–400 words.
    pub from_the_editor: String,
    /// Section name → 2–3 sentence intro.
    #[serde(default)]
    pub section_intros: BTreeMap<String, String>,
}

/// The per-article summary call's JSON response.
#[derive(Debug, Clone, Default, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    summary: String,
}

// ---------------------------------------------------------------------------
// Per-article summaries
// ---------------------------------------------------------------------------

/// One 2–3 sentence newspaper abstract: what it argues, why it's worth reading (§3.6).
pub async fn summarize_article(
    llm: &LlmClient,
    title: &str,
    body_html: &str,
    temperature: f32,
) -> Result<String, LlmError> {
    llm.meter.check_budget()?;
    let body = truncate_tokens(&prompt_text(body_html), SUMMARY_INPUT_TOKEN_BUDGET);
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
            "(no body text was extracted; summarize from the headline alone and say the \
             full text was unavailable)"
        } else {
            &body
        }
    );
    let response: SummaryResponse = llm.complete_json(&prompt, temperature).await?;
    let summary = response.summary.trim().to_string();
    if summary.is_empty() {
        return Err(LlmError::EmptyResponse);
    }
    Ok(summary)
}

/// Summarize every pick, returning `article_id → summary` (§3.6).
///
/// Stops early and returns what it has when the cost guardrail trips (§3.6).
pub async fn summarize_all(
    llm: &LlmClient,
    lineup: &Lineup,
    temperature: f32,
) -> BTreeMap<ArticleId, String> {
    let mut out = BTreeMap::new();
    for (n, pick) in lineup.picks.iter().enumerate() {
        if llm.meter.budget_exceeded() {
            tracing::error!(
                summarized = out.len(),
                remaining = lineup.picks.len() - n,
                spent_usd = llm.meter.cost_usd(),
                "COST CEILING HIT during stage C — the remaining articles fall back to \
                 feed excerpts as summaries"
            );
            break;
        }
        match summarize_article(
            llm,
            &pick.article.title,
            &pick.article.content_html,
            temperature,
        )
        .await
        {
            Ok(summary) => {
                out.insert(pick.article.id, summary);
            }
            Err(LlmError::BudgetExceeded { spent, limit }) => {
                tracing::error!(spent, limit, "COST CEILING HIT during stage C");
                break;
            }
            Err(e) => {
                tracing::warn!(
                    article_id = pick.article.id,
                    title = %pick.article.title,
                    error = %e,
                    "summary failed; falling back to the article's own opening"
                );
            }
        }
    }
    tracing::info!(
        summarized = out.len(),
        picks = lineup.picks.len(),
        "stage C summaries complete"
    );
    out
}

// ---------------------------------------------------------------------------
// Front page
// ---------------------------------------------------------------------------

/// The single front-page + section-intro call (§3.6).
pub async fn front_page(
    llm: &LlmClient,
    lineup: &Lineup,
    summaries: &BTreeMap<ArticleId, String>,
    temperature: f32,
) -> Result<FrontPageResponse, LlmError> {
    llm.meter.check_budget()?;
    let prompt = build_front_page_prompt(lineup, summaries);
    tracing::debug!(
        approx_tokens = super::approx_tokens(&prompt),
        "stage C front-page request"
    );
    let mut response: FrontPageResponse = llm.complete_json(&prompt, temperature).await?;
    response.from_the_editor = response.from_the_editor.trim().to_string();
    if response.from_the_editor.is_empty() {
        return Err(LlmError::EmptyResponse);
    }
    // Keep only intros for sections that actually exist in the issue.
    response
        .section_intros
        .retain(|name, text| lineup.section_order.contains(name) && !text.trim().is_empty());
    Ok(response)
}

/// Render the front-page user prompt: the whole lineup with its abstracts (§3.6).
pub fn build_front_page_prompt(lineup: &Lineup, summaries: &BTreeMap<ArticleId, String>) -> String {
    let mut prompt = String::with_capacity(4096);
    prompt.push_str(FRONT_PAGE_INSTRUCTIONS);
    let minutes: i64 = lineup
        .picks
        .iter()
        .map(|p| p.article.reading_minutes())
        .sum();
    let _ = write!(
        prompt,
        "\n\nISSUE: {} · {} articles across {} sections · about {} minutes of reading\n\
         SECTIONS, in order: {}\n\nLINEUP\n",
        lineup.date,
        lineup.picks.len(),
        lineup.section_order.len(),
        minutes,
        lineup.section_order.join(" | ")
    );
    for section in &lineup.section_order {
        let _ = write!(prompt, "\n## {section}\n");
        for pick in lineup.section_picks(section) {
            let _ = write!(prompt, "{}", render_pick(pick, summaries));
        }
    }
    prompt
}

fn render_pick(pick: &Pick, summaries: &BTreeMap<ArticleId, String>) -> String {
    let a = &pick.article;
    let mut block = String::with_capacity(400);
    let _ = writeln!(
        block,
        "\n- {}{}",
        a.title.trim(),
        if pick.is_lead { "  [LEAD STORY]" } else { "" }
    );
    let _ = writeln!(
        block,
        "  source: {} · {} words (~{} min){}",
        if a.feed_title.is_empty() {
            "unknown"
        } else {
            a.feed_title.trim()
        },
        a.word_count,
        a.reading_minutes(),
        social_note(pick)
    );
    let abstract_text = summaries
        .get(&a.id)
        .cloned()
        .unwrap_or_else(|| excerpt_summary(pick));
    let _ = writeln!(block, "  abstract: {abstract_text}");
    block
}

fn social_note(pick: &Pick) -> String {
    if pick.article.social.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = pick
        .article
        .social
        .iter()
        .map(|s| {
            format!(
                "{} {} pts/{} comments",
                s.source.display_name(),
                s.score,
                s.num_comments
            )
        })
        .collect();
    format!(" · {}", parts.join(", "))
}

// ---------------------------------------------------------------------------
// Fallbacks (§3.6, notes §6)
// ---------------------------------------------------------------------------

/// The article's own opening words, used when no LLM summary exists (§3.6).
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

/// A plain, factual front page used when the model is unavailable (§3.6, notes §6).
pub fn fallback_front_page_html(lineup: &Lineup) -> String {
    let minutes: i64 = lineup
        .picks
        .iter()
        .map(|p| p.article.reading_minutes())
        .sum();
    let mut text = format!(
        "Today's issue collects {} articles across {} sections — about {} minutes of \
         reading. Editorial notes are unavailable for this issue, so the lineup speaks \
         for itself.",
        lineup.picks.len(),
        lineup.section_order.len(),
        minutes
    );
    if let Some(lead) = lineup.lead() {
        let _ = write!(
            text,
            "\n\nLeading today: “{}” ({}).",
            lead.article.title.trim(),
            if lead.article.feed_title.is_empty() {
                "source unknown"
            } else {
                lead.article.feed_title.trim()
            }
        );
    }
    if !lineup.section_order.is_empty() {
        let _ = write!(
            text,
            "\n\nIn this issue: {}.",
            lineup.section_order.join(", ")
        );
    }
    text_to_paragraphs(&text)
}

/// `--skip-llm` / budget-exceeded fallback: feed excerpts stand in for summaries
/// and the front page is a plain stats line (§3.6, notes §6).
pub fn fallback_editorial(lineup: &Lineup) -> Editorial {
    Editorial {
        front_page_html: fallback_front_page_html(lineup),
        section_intros: BTreeMap::new(),
        summaries: lineup
            .picks
            .iter()
            .map(|pick| (pick.article.id, excerpt_summary(pick)))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Stage driver
// ---------------------------------------------------------------------------

/// Stage C end to end: summaries, then one front-page call, with excerpts filling
/// every gap (§3.6).
pub async fn run(llm: &LlmClient, lineup: &Lineup, temperature: f32) -> Editorial {
    if lineup.picks.is_empty() {
        return fallback_editorial(lineup);
    }

    let mut summaries = summarize_all(llm, lineup, temperature).await;
    let missing: Vec<&Pick> = lineup
        .picks
        .iter()
        .filter(|p| !summaries.contains_key(&p.article.id))
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            count = missing.len(),
            "using feed excerpts as summaries for articles the model did not cover"
        );
        for pick in missing {
            summaries.insert(pick.article.id, excerpt_summary(pick));
        }
    }

    let (front_page_html, section_intros) =
        match front_page(llm, lineup, &summaries, temperature).await {
            Ok(response) => (
                text_to_paragraphs(&response.from_the_editor),
                response.section_intros,
            ),
            Err(e) => {
                tracing::error!(error = %e,
                    "front-page generation failed; using the plain front page");
                (fallback_front_page_html(lineup), BTreeMap::new())
            }
        };

    Editorial {
        front_page_html,
        section_intros,
        summaries,
    }
}

/// Escape-and-wrap helper for callers rendering a summary straight into XHTML.
pub fn summary_to_html(summary: &str) -> String {
    format!("<p>{}</p>", escape_html(summary.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeepseekConfig;
    use crate::curate::llm::{MockBackend, UsageMeter};
    use crate::curate::prefilter::tests::article;
    use crate::types::TokenUsage;
    use std::sync::Arc;

    const FRONT_PAGE_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/deepseek_front_page.json"
    ));

    fn pick(id: i64, title: &str, section: &str, is_lead: bool) -> Pick {
        let mut a = article(id, title, 900);
        a.content_html = format!("<p>{title} opens with a specific, concrete claim.</p>");
        Pick {
            article: a,
            section: section.into(),
            position: 1,
            is_lead,
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

    fn client(backend: Arc<MockBackend>, limit: f64) -> LlmClient {
        LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), limit),
            backend,
        )
    }

    #[tokio::test]
    async fn summary_prompt_carries_headline_and_truncated_body() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"summary": "A team moves 40TB of relational data off Postgres and documents every rollback."}"#,
            TokenUsage::default(),
        );
        let llm = client(Arc::clone(&backend), 2.0);
        let body = format!("<p>{}</p>", "word ".repeat(20_000));
        let summary = summarize_article(&llm, "Migrating 40TB", &body, 0.8)
            .await
            .expect("summary");
        assert!(summary.starts_with("A team moves 40TB"));

        let prompt = &backend.prompts()[0].user;
        assert!(prompt.starts_with(SUMMARY_INSTRUCTIONS));
        assert!(prompt.contains("HEADLINE: Migrating 40TB"));
        assert!(prompt.contains("(truncated for length)"));
        // ~5k tokens ≈ 20k characters of body, not the full 100k.
        assert!(prompt.len() < 26_000, "prompt was {} bytes", prompt.len());
    }

    #[tokio::test]
    async fn front_page_parses_and_filters_unknown_sections() {
        let backend = Arc::new(MockBackend::new());
        backend.push(FRONT_PAGE_FIXTURE, TokenUsage::default());
        let llm = client(Arc::clone(&backend), 2.0);
        let lineup = lineup();
        let summaries = BTreeMap::from([
            (1, "A migration story with numbers.".to_string()),
            (2, "Transit data, charted.".to_string()),
        ]);

        let response = front_page(&llm, &lineup, &summaries, 0.8)
            .await
            .expect("front page");
        assert!(response.from_the_editor.split_whitespace().count() > 40);
        assert_eq!(response.section_intros.len(), 2);
        assert!(response.section_intros.contains_key("Top Stories"));
        assert!(
            !response.section_intros.contains_key("Niche Corner"),
            "intros for absent sections are dropped"
        );

        let prompt = &backend.prompts()[0].user;
        assert!(prompt.starts_with(FRONT_PAGE_INSTRUCTIONS));
        assert!(prompt.contains("## Top Stories"));
        assert!(prompt.contains("[LEAD STORY]"));
        assert!(prompt.contains("abstract: A migration story with numbers."));
        assert!(prompt.contains("2026-08-15"));
    }

    #[tokio::test]
    async fn full_stage_c_produces_summaries_intros_and_front_page() {
        let backend = Arc::new(MockBackend::new());
        backend.push(r#"{"summary": "First abstract."}"#, TokenUsage::default());
        backend.push(r#"{"summary": "Second abstract."}"#, TokenUsage::default());
        backend.push(FRONT_PAGE_FIXTURE, TokenUsage::default());
        let llm = client(Arc::clone(&backend), 2.0);

        let editorial = run(&llm, &lineup(), 0.8).await;
        assert_eq!(
            backend.calls(),
            3,
            "one call per article plus the front page"
        );
        assert_eq!(editorial.summaries.len(), 2);
        assert_eq!(editorial.summaries[&1], "First abstract.");
        assert!(editorial.front_page_html.starts_with("<p>"));
        assert!(editorial.front_page_html.contains("</p>"));
        assert!(!editorial.front_page_html.contains("<script"));
        assert_eq!(editorial.section_intros.len(), 2);
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
                output_tokens: 0,
            },
        );
        let llm = client(Arc::clone(&backend), 0.05);

        let editorial = run(&llm, &lineup(), 0.8).await;
        assert_eq!(backend.calls(), 1, "no further calls after the ceiling");
        assert!(llm.meter.budget_exceeded());
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
        assert!(editorial.section_intros.is_empty());
    }

    #[tokio::test]
    async fn a_failed_summary_call_is_not_fatal() {
        let backend = Arc::new(MockBackend::new());
        backend.push_error("400 bad request");
        backend.push(r#"{"summary": "Second abstract."}"#, TokenUsage::default());
        backend.push_error("500 front page exploded");
        let llm = client(Arc::clone(&backend), 2.0);

        let editorial = run(&llm, &lineup(), 0.8).await;
        assert_eq!(editorial.summaries.len(), 2);
        assert!(editorial.summaries[&1].contains("opens with a specific"));
        assert_eq!(editorial.summaries[&2], "Second abstract.");
        assert!(editorial.front_page_html.contains("Leading today"));
    }

    #[test]
    fn fallback_editorial_covers_every_pick() {
        let lineup = lineup();
        let editorial = fallback_editorial(&lineup);
        assert_eq!(editorial.summaries.len(), lineup.picks.len());
        assert!(editorial.section_intros.is_empty());
        assert!(editorial.front_page_html.contains("2 articles"));
        assert!(
            editorial
                .front_page_html
                .contains("Top Stories, Boston &amp; Local")
        );
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
