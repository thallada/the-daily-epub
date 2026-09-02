//! Stage A — batched LLM scoring (spec §3.6).
//!
//! Batches of `deepseek.score_batch_size` articles per request. Per article we
//! send title, source feed, author, word count, social stats, sources list and a
//! ~200-word excerpt; the model returns one JSON object per article.
//!
//! Parsing is deliberately forgiving: one malformed item must not cost us the
//! other eleven, and a failed batch must not fail the run.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::llm::{LlmClient, LlmError, strip_code_fence};
use super::{prompt_text, truncate_words};
use crate::types::{ArticleId, LlmScore, ScoredArticle, SourceKind};

/// Words of article text sent per candidate in stage A (§3.6).
pub const EXCERPT_WORDS: usize = 200;

/// One element of the stage-A JSON response (§3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreItem {
    pub id: ArticleId,
    /// 0–10.
    pub score: f64,
    pub category: String,
    /// ≤ 20 words.
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub is_paywalled_guess: bool,
}

impl From<ScoreItem> for LlmScore {
    fn from(i: ScoreItem) -> Self {
        LlmScore {
            score: i.score,
            category: i.category,
            rationale: i.rationale,
            is_paywalled_guess: i.is_paywalled_guess,
        }
    }
}

/// Envelope the model is asked to return (`{"articles": [...]}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreResponse {
    #[serde(default)]
    pub articles: Vec<ScoreItem>,
}

/// The invariant instruction block for stage A. Everything article-specific goes
/// in the per-batch tail so this prefix stays cacheable (§3.6).
pub const SCORE_INSTRUCTIONS: &str = "\
TASK: score a batch of candidate articles for today's issue of The Daily EPUB.

Judge each article against the reader profile in your system prompt — not against \
a general audience, and not against what is objectively newsworthy.

Return one object per input article with these fields:
  \"id\"                  integer, copied exactly from the input
  \"score\"               number 0-10, the rubric below
  \"category\"            one short label from the palette below
  \"rationale\"           at most 20 words, concrete, no hedging, no restating the title
  \"is_paywalled_guess\"  true when the text looks truncated, teaser-like or paywalled

SCORING RUBRIC — calibrate hard; a normal day averages about 4, and a 9 should \
appear a couple of times a week, not a couple of times a day:
  9-10  Exceptional. Original reporting, a deep technical dive, or an essay he \
will still be thinking about next week. Evident effort and a real point of view.
  7-8   Strong. A well-made long-form piece squarely in his interests, or an \
outstanding piece outside them.
  5-6   Worth a slot on a thin day. Solid, useful, a little thin or a little \
familiar.
  3-4   Marginal. Competent news-of-the-day, short posts, incremental updates, \
good writing about an over-covered story.
  1-2   Weak. Announcements, changelogs and release notes, link roundups, \
listicles, rewrites of a story available at the source, thin AI-industry churn.
  0     Unusable. Press releases, sponsored content, engagement bait, spam, \
pure crypto promotion, or an entry with no readable body.

CALIBRATION NOTES
- Length alone is not quality; padding scores worse than a tight short piece. But \
between two equally good pieces, prefer the one with more substance.
- Social proof is evidence, not a verdict: hundreds of HN points mean a critical \
audience read it; a quiet post from a good blog can still outrank it.
- \"came via scour\" means the story already matched one of his standing \
interests. \"came via hn_frontpage\" means it cleared HN's front page.
- Boston/New England local stories and ultra-niche community news get a genuine \
lift — this paper wants them.
- Wire-service world/US news should score low here: the World Briefing section \
covers that separately.
- Excerpt-only or paywalled text is a real cost to the reader; score it lower \
unless the piece is clearly excellent.

Return JSON exactly in this shape, with one entry per input article and nothing \
else:
{\"articles\": [{\"id\": 123, \"score\": 7.5, \"category\": \"Tech & Engineering\", \
\"rationale\": \"first-hand account of migrating 40TB off Postgres\", \
\"is_paywalled_guess\": false}]}";

/// Render the user prompt for one batch (§3.6).
pub fn build_batch_prompt(batch: &[ScoredArticle], sections: &[String]) -> String {
    let mut prompt = String::with_capacity(4096 + batch.len() * 1500);
    prompt.push_str(SCORE_INSTRUCTIONS);
    let _ = write!(
        prompt,
        "\n\nCATEGORY PALETTE (use one of these exact strings): {}\n\nARTICLES ({} in this batch)\n",
        sections.join(" | "),
        batch.len()
    );
    for candidate in batch {
        prompt.push('\n');
        prompt.push_str(&render_candidate(candidate));
    }
    prompt
}

/// One article's block in the stage-A prompt (§3.6).
fn render_candidate(candidate: &ScoredArticle) -> String {
    let a = &candidate.article;
    let mut block = String::with_capacity(1500);
    let _ = writeln!(block, "--- id: {}", a.id);
    let _ = writeln!(block, "title: {}", a.title.trim());
    let _ = writeln!(
        block,
        "feed: {}{}",
        if a.feed_title.is_empty() {
            "unknown"
        } else {
            a.feed_title.trim()
        },
        a.category
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(|c| format!(" (category: {c})"))
            .unwrap_or_default()
    );
    if let Some(author) = a.author.as_deref().filter(|s| !s.trim().is_empty()) {
        let _ = writeln!(block, "author: {}", author.trim());
    }
    let _ = writeln!(
        block,
        "length: {} words (~{} min read){}",
        a.word_count,
        a.reading_minutes(),
        if a.excerpt_only {
            " [EXCERPT ONLY — full text unavailable]"
        } else {
            ""
        }
    );
    let _ = writeln!(block, "social: {}", social_line(candidate));
    let _ = writeln!(block, "came via: {}", sources_line(candidate));
    let excerpt = truncate_words(&prompt_text(&a.content_html), EXCERPT_WORDS);
    let _ = writeln!(
        block,
        "excerpt: {}",
        if excerpt.is_empty() {
            "(no body text extracted)"
        } else {
            &excerpt
        }
    );
    block
}

fn social_line(candidate: &ScoredArticle) -> String {
    if candidate.article.social.is_empty() {
        return "none found".into();
    }
    let mut parts: Vec<String> = candidate
        .article
        .social
        .iter()
        .map(|s| {
            format!(
                "{} {} points / {} comments",
                s.source.display_name(),
                s.score,
                s.num_comments
            )
        })
        .collect();
    parts.push(format!("composite {:.2}", candidate.social_score));
    parts.join("; ")
}

fn sources_line(candidate: &ScoredArticle) -> String {
    let mut kinds: Vec<&str> = candidate
        .article
        .sources
        .iter()
        .map(|s| match s.kind {
            SourceKind::Scour => "scour",
            SourceKind::HnFrontpage => "hn_frontpage",
            SourceKind::Lobsters => "lobsters",
            SourceKind::Reddit => "reddit",
            SourceKind::Feed => "feed",
        })
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    if candidate.auto_include {
        kinds.push("always-include feed (cannot be dropped)");
    }
    if kinds.is_empty() {
        "feed".into()
    } else {
        kinds.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Response parsing (§3.6: tolerate anything the model does to us)
// ---------------------------------------------------------------------------

/// Keys the model might wrap the array in, in preference order.
const ARRAY_KEYS: &[&str] = &["articles", "scores", "results", "items", "data"];

/// Parse a stage-A response leniently: missing optional fields default, scores
/// are clamped to 0–10, and malformed items are skipped with a warning (§3.6).
pub fn parse_score_response(raw: &str) -> Vec<ScoreItem> {
    let cleaned = strip_code_fence(raw);
    let value: Value = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "stage A response was not JSON at all");
            return Vec::new();
        }
    };

    let array = match &value {
        Value::Array(items) => Some(items),
        Value::Object(map) => ARRAY_KEYS
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_array))
            // Some models return {"1234": {...}} or a single bare object.
            .or_else(|| map.values().find_map(Value::as_array)),
        _ => None,
    };
    let Some(array) = array else {
        tracing::warn!("stage A response contained no array of scores");
        return Vec::new();
    };

    let mut out = Vec::with_capacity(array.len());
    let mut skipped = 0usize;
    for item in array {
        match parse_item(item) {
            Some(parsed) => out.push(parsed),
            None => {
                skipped += 1;
                tracing::warn!(item = %truncate_debug(item), "skipping malformed stage A item");
            }
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, kept = out.len(), "stage A items were dropped");
    }
    out
}

fn parse_item(item: &Value) -> Option<ScoreItem> {
    let obj = item.as_object()?;
    let id = obj.get("id").and_then(as_i64_lenient)?;
    let score = obj
        .get("score")
        .and_then(as_f64_lenient)
        .or_else(|| obj.get("rating").and_then(as_f64_lenient))?;
    Some(ScoreItem {
        id,
        score: score.clamp(0.0, 10.0),
        category: obj
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        rationale: obj
            .get("rationale")
            .or_else(|| obj.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        is_paywalled_guess: obj
            .get("is_paywalled_guess")
            .or_else(|| obj.get("paywalled"))
            .and_then(as_bool_lenient)
            .unwrap_or(false),
    })
}

fn as_i64_lenient(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn as_f64_lenient(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|f| f.is_finite())
}

fn as_bool_lenient(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| match v.as_str()?.trim() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    })
}

fn truncate_debug(v: &Value) -> String {
    v.to_string().chars().take(160).collect()
}

// ---------------------------------------------------------------------------
// Stage driver
// ---------------------------------------------------------------------------

/// Score every candidate, filling in [`ScoredArticle::llm`] (§3.6).
///
/// Batches that fail are logged and left unscored rather than aborting the run.
/// Returns how many candidates came back with a score.
pub async fn score_all(
    llm: &LlmClient,
    candidates: &mut [ScoredArticle],
    batch_size: usize,
    sections: &[String],
    temperature: f32,
) -> Result<usize, LlmError> {
    if candidates.is_empty() {
        return Ok(0);
    }
    let batch_size = batch_size.max(1);
    let batches = candidates.len().div_ceil(batch_size);
    let mut scores: HashMap<ArticleId, LlmScore> = HashMap::with_capacity(candidates.len());

    for (n, batch) in candidates.chunks(batch_size).enumerate() {
        if let Err(e) = llm.meter.check_budget() {
            tracing::error!(
                error = %e,
                batch = n + 1,
                of = batches,
                unscored = candidates.len() - scores.len(),
                "COST CEILING HIT during stage A scoring — remaining batches skipped; \
                 the lineup will fall back to heuristic ranking for them"
            );
            break;
        }
        let prompt = build_batch_prompt(batch, sections);
        tracing::debug!(
            batch = n + 1,
            of = batches,
            articles = batch.len(),
            approx_tokens = super::approx_tokens(&prompt),
            "stage A request"
        );
        match llm.complete(&prompt, temperature, true).await {
            Ok(raw) => {
                let items = parse_score_response(&raw);
                if items.is_empty() {
                    tracing::warn!(
                        batch = n + 1,
                        of = batches,
                        "stage A batch returned no scores"
                    );
                }
                for item in items {
                    scores.insert(item.id, item.into());
                }
            }
            Err(e) => {
                tracing::warn!(batch = n + 1, of = batches, error = %e,
                    "stage A batch failed; its articles stay unscored");
            }
        }
    }

    let mut applied = 0usize;
    for candidate in candidates.iter_mut() {
        if let Some(score) = scores.remove(&candidate.article.id) {
            candidate.llm = Some(score);
            applied += 1;
        }
    }
    if !scores.is_empty() {
        tracing::warn!(
            unknown_ids = scores.len(),
            "stage A returned scores for ids that were not in the batch"
        );
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeepseekConfig;
    use crate::curate::llm::{MockBackend, UsageMeter};
    use crate::curate::prefilter::tests::{article, via, with_social};
    use crate::types::TokenUsage;
    use std::sync::Arc;

    const BATCH_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/deepseek_score_batch.json"
    ));
    const MESSY_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/deepseek_score_batch_messy.json"
    ));

    fn sections() -> Vec<String> {
        crate::config::CurationConfig::default().sections
    }

    fn candidate(id: i64, title: &str, words: i64) -> ScoredArticle {
        ScoredArticle {
            article: article(id, title, words),
            prefilter_score: 50.0,
            social_score: 0.0,
            llm: None,
            auto_include: false,
        }
    }

    #[test]
    fn batch_prompt_carries_every_documented_signal() {
        let mut c = candidate(12, "Migrating 40TB off Postgres", 3200);
        c.article = via(
            with_social(c.article, 342, 210),
            SourceKind::HnFrontpage,
            9001,
        );
        c.social_score = c.article.social_score();
        c.auto_include = true;
        let prompt = build_batch_prompt(&[c], &sections());

        assert!(prompt.starts_with(SCORE_INSTRUCTIONS));
        assert!(prompt.contains("--- id: 12"));
        assert!(prompt.contains("title: Migrating 40TB off Postgres"));
        assert!(prompt.contains("feed: Some Blog (category: Tech)"));
        assert!(prompt.contains("author: A. Writer"));
        assert!(prompt.contains("length: 3200 words"));
        assert!(prompt.contains("HN 342 points / 210 comments"));
        assert!(prompt.contains("hn_frontpage"));
        assert!(prompt.contains("always-include feed"));
        assert!(prompt.contains("excerpt: word word"));
        assert!(prompt.contains("Tech & Engineering"));
        // The excerpt is capped.
        let excerpt_line = prompt
            .lines()
            .find(|l| l.starts_with("excerpt:"))
            .expect("excerpt line");
        assert!(excerpt_line.split_whitespace().count() <= EXCERPT_WORDS + 2);
    }

    #[test]
    fn parses_a_realistic_deepseek_batch() {
        let items = parse_score_response(BATCH_FIXTURE);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].id, 101);
        assert!((items[0].score - 8.5).abs() < 1e-9);
        assert_eq!(items[0].category, "Tech & Engineering");
        assert!(items[0].rationale.split_whitespace().count() <= 20);
        assert!(!items[0].is_paywalled_guess);
        assert!(items[3].is_paywalled_guess);
        let score: LlmScore = items[0].clone().into();
        assert_eq!(score.category, "Tech & Engineering");
    }

    #[test]
    fn parsing_survives_everything_a_model_might_do() {
        let items = parse_score_response(MESSY_FIXTURE);
        let ids: Vec<ArticleId> = items.iter().map(|i| i.id).collect();
        // 201 fine; 202 string score clamped; 203 missing rationale/category;
        // 204 out-of-range clamped; the two malformed entries are dropped.
        assert_eq!(ids, vec![201, 202, 203, 204]);
        assert!((items[1].score - 6.0).abs() < 1e-9);
        assert_eq!(items[2].rationale, "");
        assert_eq!(items[2].category, "");
        assert!(
            (items[3].score - 10.0).abs() < 1e-9,
            "clamped to the 0-10 range"
        );
        assert!(items.iter().all(|i| (0.0..=10.0).contains(&i.score)));
    }

    #[test]
    fn parsing_tolerates_fences_arrays_and_junk() {
        assert_eq!(
            parse_score_response("```json\n{\"articles\":[{\"id\":1,\"score\":5}]}\n```").len(),
            1
        );
        assert_eq!(parse_score_response("[{\"id\": 2, \"score\": 3}]").len(), 1);
        assert_eq!(
            parse_score_response("{\"results\":[{\"id\":3,\"score\":\"4.5\"}]}")[0].score,
            4.5
        );
        assert!(parse_score_response("I'm sorry, I can't do that").is_empty());
        assert!(parse_score_response("{\"articles\": {}}").is_empty());
    }

    fn client(backend: Arc<MockBackend>, limit_usd: f64) -> LlmClient {
        LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), limit_usd),
            backend,
        )
    }

    #[tokio::test]
    async fn scores_are_applied_batch_by_batch() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"articles":[{"id":1,"score":8,"category":"Tech & Engineering","rationale":"good"},
                            {"id":2,"score":2,"category":"Niche Corner","rationale":"thin"}]}"#,
            TokenUsage::default(),
        );
        backend.push(
            r#"{"articles":[{"id":3,"score":6.5,"category":"Culture & Essays","rationale":"solid"}]}"#,
            TokenUsage::default(),
        );
        let llm = client(Arc::clone(&backend), 2.0);

        let mut candidates = vec![
            candidate(1, "One", 1000),
            candidate(2, "Two", 1000),
            candidate(3, "Three", 1000),
        ];
        let scored = score_all(&llm, &mut candidates, 2, &sections(), 0.3)
            .await
            .expect("scoring");
        assert_eq!(scored, 3);
        assert_eq!(backend.calls(), 2, "batched by score_batch_size");
        assert_eq!(candidates[0].llm.as_ref().map(|l| l.score), Some(8.0));
        assert_eq!(candidates[2].llm.as_ref().map(|l| l.score), Some(6.5));
        // combined_score now reflects the LLM verdict.
        assert!(candidates[0].combined_score() > candidates[1].combined_score());
    }

    #[tokio::test]
    async fn a_failed_batch_does_not_sink_the_run() {
        let backend = Arc::new(MockBackend::new());
        backend.push_error("500 upstream exploded");
        backend.push(
            r#"{"articles":[{"id":2,"score":7,"category":"Top Stories","rationale":"ok"}]}"#,
            TokenUsage::default(),
        );
        let llm = client(Arc::clone(&backend), 2.0);
        let mut candidates = vec![candidate(1, "One", 900), candidate(2, "Two", 900)];
        let scored = score_all(&llm, &mut candidates, 1, &sections(), 0.3)
            .await
            .expect("scoring must not abort");
        assert_eq!(scored, 1);
        assert!(candidates[0].llm.is_none());
        assert!(candidates[1].llm.is_some());
    }

    #[tokio::test]
    async fn scoring_stops_when_the_budget_is_gone() {
        let backend = Arc::new(MockBackend::new());
        // First batch alone blows a $0.05 ceiling ($0.14 per 1M input tokens).
        backend.push(
            r#"{"articles":[{"id":1,"score":9,"category":"Top Stories","rationale":"great"}]}"#,
            TokenUsage {
                input_tokens: 1_000_000,
                cached_tokens: 0,
                output_tokens: 0,
            },
        );
        backend.push(
            r#"{"articles":[{"id":2,"score":9,"category":"Top Stories","rationale":"great"}]}"#,
            TokenUsage::default(),
        );
        let llm = client(Arc::clone(&backend), 0.05);
        let mut candidates = vec![candidate(1, "One", 900), candidate(2, "Two", 900)];
        let scored = score_all(&llm, &mut candidates, 1, &sections(), 0.3)
            .await
            .expect("scoring");
        assert_eq!(scored, 1, "only the first batch ran");
        assert_eq!(backend.calls(), 1);
        assert!(llm.meter.budget_exceeded());
    }
}
