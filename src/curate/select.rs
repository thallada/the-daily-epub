//! The editor — lineup selection (plan §13).
//!
//! One call on the editor client (Claude), falling back to the same prompt on the
//! bulk client (DeepSeek), then to [`select_without_llm`]. The shortlist is the
//! top candidates by [`ScoredArticle::combined_score`] with their Stage A
//! rationales; the model returns picks, each with a section from the configured
//! palette, an ordering, exactly one `lead_story`, and a one-line `why` that is
//! printed under the headline.
//!
//! The model's answer is treated as a proposal, never as gospel: sections are
//! validated against the palette, the lead is forced to be unique, auto-include
//! feeds are re-inserted if they were dropped, duplicate ids are dropped, and the
//! size is trimmed to `hard_max`. There is **no minimum**: a nine-pick answer is
//! published as nine (the "top up" branch is gone). `--max-articles N` is a
//! ceiling: `hard_max = min(curation.max_article_count, N)` and
//! `soft_target = min(target_article_count, hard_max)`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use super::llm::{LlmError, Llms, strip_code_fence};
use super::{prompt_text, truncate_words};
use crate::types::{ArticleId, Lineup, Pick, ScoredArticle, WORLD_BRIEFING_SECTION};

/// Words of lead-in text shown per candidate in the editor prompt (§13).
const BLURB_WORDS: usize = 60;

/// One element of the editor's JSON response (§13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionItem {
    pub id: ArticleId,
    /// Must be one of `curation.sections`.
    pub section: String,
    pub position: i64,
    #[serde(default)]
    pub lead_story: bool,
    #[serde(default)]
    pub why: Option<String>,
}

/// Envelope the model is asked to return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionResponse {
    #[serde(default)]
    pub picks: Vec<SelectionItem>,
}

/// The invariant instruction block for the editor (§13), with `{soft_target}` and
/// `{hard_max}` substituted at render time.
pub const EDITOR_INSTRUCTIONS: &str = r#"TASK: assemble today's issue of The Daily EPUB from the shortlist below.

You are choosing what one specific reader — the profile, learned adjustments and
recent verdicts in your system prompt — will read on an e-ink screen over breakfast.
Build a paper, not a ranking: it should have a shape, a range of subjects, and a
clear front page.

RULES
1. Pick by id from the shortlist only.
2. Every pick gets a section from the palette, spelled exactly.
3. Number picks within a section from 1, best first.
4. Exactly one pick is "lead_story": true, in the first section you use.
5. Candidates flagged always-include MUST appear.
6. Never select two articles that tell the same story.
7. SIZE: aim for about {soft_target}; never more than {hard_max}; there is NO minimum.
   If only nine pieces deserve the reader's morning, publish nine. Never pad.
8. For every pick write "why": at most 14 words, specific to this article and this
   reader, in the second person is fine ("the Postgres failover story you'd argue with").
   It is printed under the headline.

EDITORIAL JUDGEMENT
- Depth over coverage. Drop anything you would not defend to him in person.
- Diversity is a feature: do not let one subject, one format, or one feed dominate,
  even if it is what he has been loving lately. A paper of eight AI posts is a failure
  even if each is good. The "recent verdicts" tell you his taste; they do not tell you
  to repeat it.
- Keep the local and ultra-niche picks when they are good; they are worth more here
  than a third industry item.
- Candidates flagged exploration were included on purpose to test the edges of his
  taste; take one if it is genuinely good, ignore it otherwise.
- Scores are evidence, not instructions. Overrule them when the paper reads better.

Return JSON exactly:
{"picks": [{"id": 123, "section": "Top Stories", "position": 1, "lead_story": true, "why": "…"}]}"#;

/// Render the editor's user prompt (§13).
pub fn build_prompt(
    shortlist: &[ScoredArticle],
    sections: &[String],
    soft_target: usize,
    hard_max: usize,
) -> String {
    let instructions = EDITOR_INSTRUCTIONS
        .replace("{soft_target}", &soft_target.to_string())
        .replace("{hard_max}", &hard_max.to_string());
    let mut prompt = String::with_capacity(2048 + shortlist.len() * 400);
    prompt.push_str(&instructions);
    let _ = write!(
        prompt,
        "\n\nSECTION PALETTE (exact strings, use only these): {}\n\
         Reserved and unavailable: \"{WORLD_BRIEFING_SECTION}\" is compiled separately.\n\n\
         SHORTLIST ({} candidates, best-ranked first)\n",
        sections.join(" | "),
        shortlist.len()
    );
    for candidate in shortlist {
        prompt.push('\n');
        prompt.push_str(&render_candidate(candidate));
    }
    prompt
}

fn render_candidate(candidate: &ScoredArticle) -> String {
    let a = &candidate.article;
    let mut block = String::with_capacity(400);
    let _ = writeln!(block, "--- id: {}", a.id);
    let _ = writeln!(block, "title: {}", a.title.trim());
    let _ = writeln!(
        block,
        "feed: {} · {} words (~{} min){}",
        if a.feed_title.is_empty() {
            "unknown"
        } else {
            a.feed_title.trim()
        },
        a.word_count,
        a.reading_minutes(),
        if a.excerpt_only {
            " · EXCERPT ONLY"
        } else {
            ""
        }
    );
    match candidate.llm.as_ref() {
        Some(llm) => {
            let _ = writeln!(
                block,
                "score: {:.1} · {} — {}",
                llm.score,
                if llm.category.is_empty() {
                    "uncategorized"
                } else {
                    llm.category.as_str()
                },
                llm.rationale.trim()
            );
        }
        None => {
            let _ = writeln!(block, "score: unscored");
        }
    }
    if let Some(triage) = candidate.triage.as_ref() {
        let _ = writeln!(
            block,
            "triage: {:.1} · {} — {}",
            triage.interest,
            triage.kind,
            triage.why.trim()
        );
    }
    let mut flags = Vec::new();
    if candidate.auto_include {
        flags.push("always-include");
    }
    if candidate.exploration {
        flags.push("exploration");
    }
    if a.excerpt_only {
        flags.push("excerpt only");
    }
    if !flags.is_empty() {
        let _ = writeln!(block, "flags: {}", flags.join(" | "));
    }
    let blurb = truncate_words(&prompt_text(&a.content_html), BLURB_WORDS);
    if !blurb.is_empty() {
        let _ = writeln!(block, "opening: {blurb}");
    }
    block
}

// ---------------------------------------------------------------------------
// Section validation (§3.6: the model may only use the configured palette)
// ---------------------------------------------------------------------------

/// The section unrecognized labels fall back to.
pub fn default_section(sections: &[String]) -> String {
    sections
        .iter()
        .find(|s| s.as_str() == "Top Stories")
        .or_else(|| sections.first())
        .cloned()
        .unwrap_or_else(|| "Top Stories".to_string())
}

/// Map whatever the model said onto the configured palette (§3.6).
///
/// Exact match → case-insensitive match → best word-overlap match → default.
pub fn resolve_section(raw: &str, sections: &[String]) -> String {
    let candidate = raw.trim();
    if candidate.is_empty() || candidate.eq_ignore_ascii_case(WORLD_BRIEFING_SECTION) {
        return default_section(sections);
    }
    if let Some(exact) = sections.iter().find(|s| s.as_str() == candidate) {
        return exact.clone();
    }
    if let Some(ci) = sections.iter().find(|s| s.eq_ignore_ascii_case(candidate)) {
        return ci.clone();
    }
    let wanted = words_of(candidate);
    let best = sections
        .iter()
        .map(|s| (s, words_of(s).intersection(&wanted).count()))
        .filter(|(_, overlap)| *overlap > 0)
        .max_by_key(|(_, overlap)| *overlap);
    match best {
        Some((section, _)) => {
            tracing::debug!(raw = candidate, mapped = %section, "mapped an off-palette section");
            section.clone()
        }
        None => {
            tracing::warn!(raw = candidate, "unknown section; using the default");
            default_section(sections)
        }
    }
}

fn words_of(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !w.eq_ignore_ascii_case("and"))
        .map(str::to_lowercase)
        .collect()
}

/// Section guess from feed metadata, used by [`select_without_llm`] (§3.6).
pub fn heuristic_section(candidate: &ScoredArticle, sections: &[String]) -> String {
    if candidate.auto_include {
        return resolve_section("From the Blogroll", sections);
    }
    let haystack = format!(
        "{} {} {}",
        candidate.article.category.as_deref().unwrap_or_default(),
        candidate.article.feed_title,
        candidate.article.title
    )
    .to_lowercase();

    const RULES: &[(&str, &[&str])] = &[
        (
            "Boston & Local",
            &[
                "boston",
                "massachusetts",
                "cambridge",
                "mbta",
                "new england",
            ],
        ),
        (
            "AI & Machine Learning",
            &[
                " ai ",
                "ai:",
                "llm",
                "machine learning",
                "neural",
                "openai",
                "anthropic",
                "gpt",
                "diffusion",
            ],
        ),
        (
            "Science & Space",
            &[
                "science",
                "space",
                "nasa",
                "astronom",
                "physics",
                "biology",
                "climate",
                "aerospace",
            ],
        ),
        (
            "Culture & Essays",
            &[
                "essay", "culture", "book", "fiction", "poetry", "film", "music", "art", "review",
            ],
        ),
        (
            "Niche Corner",
            &[
                "hobby",
                "retro",
                "keyboard",
                "board game",
                "coffee",
                "e-ink",
                "eink",
            ],
        ),
        (
            "Tech & Engineering",
            &[
                "tech",
                "programming",
                "engineering",
                "software",
                "developer",
                "rust",
                "linux",
                "database",
                "systems",
                "web",
            ],
        ),
    ];
    for (section, needles) in RULES {
        if needles.iter().any(|n| haystack.contains(n))
            && let Some(found) = sections.iter().find(|s| s.as_str() == *section)
        {
            return found.clone();
        }
    }
    default_section(sections)
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Keys the model might wrap the array in.
const ARRAY_KEYS: &[&str] = &["picks", "lineup", "articles", "selection", "items"];

/// Lenient parse of the editor response (§13). `why` is capped at 14 words.
pub fn parse_selection_response(raw: &str) -> Vec<SelectionItem> {
    let cleaned = strip_code_fence(raw);
    let value: serde_json::Value = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "stage B response was not JSON");
            return Vec::new();
        }
    };
    let array = match &value {
        serde_json::Value::Array(items) => Some(items),
        serde_json::Value::Object(map) => ARRAY_KEYS
            .iter()
            .find_map(|k| map.get(*k).and_then(serde_json::Value::as_array))
            .or_else(|| map.values().find_map(serde_json::Value::as_array)),
        _ => None,
    };
    let Some(array) = array else {
        tracing::warn!("stage B response contained no array of picks");
        return Vec::new();
    };

    let mut out = Vec::with_capacity(array.len());
    for (idx, item) in array.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            tracing::warn!("skipping a non-object stage B pick");
            continue;
        };
        let Some(id) = obj.get("id").and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        }) else {
            tracing::warn!("skipping a stage B pick without an id");
            continue;
        };
        out.push(SelectionItem {
            id,
            section: obj
                .get("section")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
            position: obj
                .get("position")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .unwrap_or(idx as i64 + 1),
            lead_story: obj
                .get("lead_story")
                .or_else(|| obj.get("is_lead"))
                .and_then(|v| {
                    v.as_bool()
                        .or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true")))
                })
                .unwrap_or(false),
            why: obj
                .get("why")
                .and_then(serde_json::Value::as_str)
                .map(|why| {
                    why.split_whitespace()
                        .take(14)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|why| !why.is_empty()),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Stage driver
// ---------------------------------------------------------------------------

/// Ask the editor for the day's lineup (§13).
///
/// Editor first, then the same prompt on the bulk client, then
/// [`select_without_llm`]; never an error unless a mock is misconfigured.
pub async fn select(
    llms: &Llms,
    candidates: Vec<ScoredArticle>,
    sections: &[String],
    soft_target: usize,
    hard_max: usize,
    date: Date,
) -> Result<Lineup, LlmError> {
    if candidates.is_empty() {
        return Ok(Lineup {
            date,
            picks: Vec::new(),
            section_order: Vec::new(),
        });
    }
    let Some(primary) = llms.editor_or_bulk() else {
        return Ok(select_without_llm(
            candidates,
            sections,
            soft_target,
            hard_max,
            date,
        ));
    };
    let shortlist = shortlist(&candidates, hard_max);
    let prompt = build_prompt(&shortlist, sections, soft_target, hard_max);
    tracing::debug!(
        shortlist = shortlist.len(),
        approx_tokens = super::approx_tokens(&prompt),
        "editor request"
    );

    let raw = match complete_with_fallback(llms, primary, &prompt).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::error!(%error, "editor and bulk fallback both failed; selecting heuristically");
            return Ok(select_without_llm(
                candidates,
                sections,
                soft_target,
                hard_max,
                date,
            ));
        }
    };
    let items = parse_selection_response(&raw);
    if items.is_empty() {
        tracing::error!("editor returned no usable picks; falling back to heuristic ranking");
        return Ok(select_without_llm(
            candidates,
            sections,
            soft_target,
            hard_max,
            date,
        ));
    }

    let by_id: HashMap<ArticleId, &ScoredArticle> =
        candidates.iter().map(|c| (c.article.id, c)).collect();
    let mut chosen = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for item in items {
        if !seen.insert(item.id) {
            tracing::warn!(id = item.id, "editor picked the same article twice");
            continue;
        }
        match by_id.get(&item.id) {
            Some(candidate) => chosen.push((item, (*candidate).clone())),
            None => tracing::warn!(id = item.id, "editor invented an id that was not offered"),
        }
    }
    for candidate in &candidates {
        if candidate.auto_include && seen.insert(candidate.article.id) {
            chosen.push((
                SelectionItem {
                    id: candidate.article.id,
                    section: "From the Blogroll".into(),
                    position: i64::MAX,
                    lead_story: false,
                    why: Some("A standing source you always want represented".into()),
                },
                candidate.clone(),
            ));
        }
    }
    Ok(assemble(chosen, sections, hard_max, date))
}

/// The editor runs at the scoring temperature: this is a judgement call, not
/// prose. The Anthropic backend ignores it (§4.2).
const EDITOR_TEMPERATURE: f32 = 0.4;

/// One attempt on `primary`; on any error (refusal, budget, API) the same prompt
/// goes to the bulk client when that is a different provider (§13, §17).
async fn complete_with_fallback(
    llms: &Llms,
    primary: &super::llm::LlmClient,
    prompt: &str,
) -> Result<String, LlmError> {
    match primary.complete(prompt, EDITOR_TEMPERATURE, true).await {
        Ok(raw) => Ok(raw),
        Err(primary_error) => {
            let fallback = llms
                .bulk
                .as_ref()
                .filter(|bulk| primary.provider != bulk.provider);
            let Some(fallback) = fallback else {
                return Err(primary_error);
            };
            tracing::warn!(
                error = %primary_error,
                provider = primary.provider,
                "editor failed; retrying the same prompt on bulk"
            );
            fallback.complete(prompt, EDITOR_TEMPERATURE, true).await
        }
    }
}

/// Step 4 offers the entire admitted deep set to the editor. Step 5 replaces
/// this with the diversified shortlist.
fn shortlist(candidates: &[ScoredArticle], _target: usize) -> Vec<ScoredArticle> {
    let mut ranked: Vec<ScoredArticle> = candidates.to_vec();
    sort_by_combined(&mut ranked);
    ranked
}

fn sort_by_combined(candidates: &mut [ScoredArticle]) {
    candidates.sort_by(|a, b| {
        b.combined_score()
            .partial_cmp(&a.combined_score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.article.id.cmp(&b.article.id))
    });
}

/// Turn validated picks into a [`Lineup`]: trim to `hard_max`, force a single
/// lead, order the sections and renumber positions (§13). No minimum size.
fn assemble(
    mut chosen: Vec<(SelectionItem, ScoredArticle)>,
    sections: &[String],
    hard_max: usize,
    date: Date,
) -> Lineup {
    // Too many: drop the weakest non-auto-include picks.
    if chosen.len() > hard_max {
        chosen.sort_by(|a, b| {
            b.1.auto_include.cmp(&a.1.auto_include).then_with(|| {
                b.1.combined_score()
                    .partial_cmp(&a.1.combined_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let dropped = chosen.len() - hard_max;
        chosen.truncate(hard_max);
        tracing::info!(dropped, hard_max, "trimmed the lineup to the size ceiling");
    }

    // Normalize sections and pick the section order.
    for (item, _) in chosen.iter_mut() {
        item.section = resolve_section(&item.section, sections);
    }
    let used: HashSet<&str> = chosen.iter().map(|(i, _)| i.section.as_str()).collect();
    let mut section_order: Vec<String> = sections
        .iter()
        .filter(|s| used.contains(s.as_str()))
        .cloned()
        .collect();
    for (item, _) in &chosen {
        if !section_order.contains(&item.section) {
            section_order.push(item.section.clone());
        }
    }
    let section_rank: HashMap<&str, usize> = section_order
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();

    // Order: section, then the model's position, then quality, then id.
    chosen.sort_by(|a, b| {
        section_rank
            .get(a.0.section.as_str())
            .cmp(&section_rank.get(b.0.section.as_str()))
            .then_with(|| a.0.position.cmp(&b.0.position))
            .then_with(|| {
                b.1.combined_score()
                    .partial_cmp(&a.1.combined_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.1.article.id.cmp(&b.1.article.id))
    });

    // Exactly one lead, and it must live in the first section (§3.6 rule 4).
    let lead_id = chosen
        .iter()
        .find(|(item, _)| item.lead_story)
        .filter(|(item, _)| section_rank.get(item.section.as_str()) == Some(&0))
        .or_else(|| chosen.first())
        .map(|(item, _)| item.id);

    let mut per_section: BTreeMap<String, i64> = BTreeMap::new();
    let picks = chosen
        .into_iter()
        .map(|(item, candidate)| {
            let position = per_section
                .entry(item.section.clone())
                .and_modify(|n| *n += 1)
                .or_insert(1);
            Pick {
                section: item.section,
                position: *position,
                is_lead: Some(item.id) == lead_id,
                why: item.why,
                summary: None,
                llm: candidate.llm.clone(),
                discussion: None,
                article: candidate.article,
            }
        })
        .collect();

    Lineup {
        date,
        picks,
        section_order,
    }
}

/// Heuristic fallback (`--skip-llm`, no provider, or both providers failed): the
/// top `soft_target` by prefilter score plus the auto-includes, bucketed into
/// sections by feed category, trimmed to `hard_max` (notes §6).
pub fn select_without_llm(
    candidates: Vec<ScoredArticle>,
    sections: &[String],
    soft_target: usize,
    hard_max: usize,
    date: Date,
) -> Lineup {
    let mut ranked = candidates;
    ranked.sort_by(|left, right| {
        right
            .prefilter_score
            .total_cmp(&left.prefilter_score)
            .then_with(|| left.article.id.cmp(&right.article.id))
    });
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    for candidate in ranked {
        if chosen.len() >= soft_target && !candidate.auto_include {
            continue;
        }
        if !seen.insert(candidate.article.id) {
            continue;
        }
        chosen.push((
            SelectionItem {
                id: candidate.article.id,
                section: heuristic_section(&candidate, sections),
                position: chosen.len() as i64 + 1,
                lead_story: false,
                why: None,
            },
            candidate,
        ));
    }
    assemble(chosen, sections, hard_max, date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AnthropicConfig, CurationConfig, DeepseekConfig};
    use crate::curate::llm::{ChatBackend, LlmClient, MockBackend, PriceTable, UsageMeter};
    use crate::curate::prefilter::tests::article;
    use crate::types::{LlmScore, TokenUsage};
    use std::sync::Arc;

    const LINEUP_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/deepseek_lineup.json"
    ));

    fn sections() -> Vec<String> {
        CurationConfig::default().sections
    }

    fn date() -> Date {
        "2026-08-15".parse().expect("date")
    }

    fn candidate(id: i64, title: &str, words: i64, score: f64) -> ScoredArticle {
        ScoredArticle {
            article: article(id, title, words),
            prefilter_score: 40.0 + score,
            social_score: 1.0,
            llm: Some(LlmScore {
                score,
                category: "Tech & Engineering".into(),
                rationale: "solid".into(),
                is_paywalled_guess: false,
            }),
            triage: None,
            auto_include: false,
            exploration: false,
            admitted_by: Vec::new(),
        }
    }

    fn candidates(n: i64) -> Vec<ScoredArticle> {
        (1..=n)
            .map(|i| {
                candidate(
                    i,
                    &format!("Article {i}"),
                    500 + i * 10,
                    (10.0 - i as f64 * 0.1).max(0.0),
                )
            })
            .collect()
    }

    fn mock(provider: &'static str, backend: Arc<MockBackend>, limit: f64) -> LlmClient {
        let prices = if provider == "anthropic" {
            PriceTable::anthropic(&AnthropicConfig::default())
        } else {
            PriceTable::deepseek(&DeepseekConfig::default())
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

    /// DeepSeek only — the shape of a run without an Anthropic key.
    fn bulk_only(backend: Arc<MockBackend>) -> Llms {
        Llms {
            bulk: Some(mock("deepseek", backend, 2.0)),
            editor: None,
        }
    }

    fn editor_and_bulk(editor: Arc<MockBackend>, bulk: Arc<MockBackend>) -> Llms {
        Llms {
            bulk: Some(mock("deepseek", bulk, 2.0)),
            editor: Some(mock("anthropic", editor, 3.0)),
        }
    }

    fn picks_json(n: i64) -> String {
        let picks: Vec<String> = (1..=n)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"section":"Top Stories","position":{i},"lead_story":{},"why":"pick {i} because"}}"#,
                    i == 1
                )
            })
            .collect();
        format!(r#"{{"picks":[{}]}}"#, picks.join(","))
    }

    #[test]
    fn section_resolution_maps_onto_the_palette() {
        let s = sections();
        assert_eq!(resolve_section("Top Stories", &s), "Top Stories");
        assert_eq!(resolve_section("  top stories ", &s), "Top Stories");
        assert_eq!(
            resolve_section("Technology & Engineering", &s),
            "Tech & Engineering"
        );
        assert_eq!(resolve_section("Science", &s), "Science & Space");
        assert_eq!(resolve_section("Sports", &s), "Top Stories");
        assert_eq!(resolve_section("", &s), "Top Stories");
        // The reserved section is never allowed through (§3.6).
        assert_eq!(resolve_section(WORLD_BRIEFING_SECTION, &s), "Top Stories");
        // A palette without "Top Stories" falls back to its first entry.
        let tiny = vec!["Niche Corner".to_string()];
        assert_eq!(resolve_section("Whatever", &tiny), "Niche Corner");
    }

    #[test]
    fn heuristic_sections_follow_feed_metadata() {
        let s = sections();
        let mut c = candidate(1, "MBTA slow zones, charted", 900, 6.0);
        c.article.category = Some("News".into());
        assert_eq!(heuristic_section(&c, &s), "Boston & Local");

        let mut ai = candidate(2, "A new LLM benchmark", 900, 6.0);
        ai.article.category = Some("Machine Learning".into());
        assert_eq!(heuristic_section(&ai, &s), "AI & Machine Learning");

        let mut blog = candidate(3, "Notes from my week", 900, 6.0);
        blog.auto_include = true;
        assert_eq!(heuristic_section(&blog, &s), "From the Blogroll");

        let mut plain = candidate(4, "Untitled musing", 900, 6.0);
        plain.article.category = None;
        plain.article.feed_title = "A Journal".into();
        assert_eq!(heuristic_section(&plain, &s), "Top Stories");
    }

    #[test]
    fn parses_a_realistic_lineup_response() {
        let items = parse_selection_response(LINEUP_FIXTURE);
        assert_eq!(items.len(), 6);
        assert_eq!(items[0].id, 101);
        assert!(items[0].lead_story);
        assert_eq!(items[0].section, "Top Stories");
        assert_eq!(items.iter().filter(|i| i.lead_story).count(), 1);
        assert!(items[0].why.as_deref().is_some_and(|w| !w.is_empty()));
        // Junk entries in the fixture are dropped, not fatal.
        assert!(items.iter().all(|i| i.id != 0));
    }

    #[test]
    fn why_lines_are_optional_and_capped_at_fourteen_words() {
        let long = (1..=30)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let items = parse_selection_response(&format!(
            r#"{{"picks":[{{"id":1,"section":"Top Stories","why":"{long}"}},
                          {{"id":2,"section":"Top Stories","why":"   "}},
                          {{"id":3,"section":"Top Stories"}}]}}"#
        ));
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0]
                .why
                .as_deref()
                .map(|w| w.split_whitespace().count()),
            Some(14)
        );
        assert!(items[1].why.is_none());
        assert!(items[2].why.is_none());
    }

    #[test]
    fn the_prompt_substitutes_the_size_targets() {
        let prompt = build_prompt(&candidates(3), &sections(), 6, 11);
        assert!(prompt.contains("aim for about 6; never more than 11; there is NO minimum"));
        assert!(!prompt.contains("{soft_target}") && !prompt.contains("{hard_max}"));
        assert!(prompt.contains("--- id: 1\n"));
        assert!(prompt.contains("score: 9.9 · Tech & Engineering — solid"));
        assert!(prompt.contains("opening: "));
        assert!(
            !prompt.contains("combined"),
            "the numeric blend stays out of the prompt"
        );
        let mut flagged = candidates(1);
        flagged[0].auto_include = true;
        flagged[0].exploration = true;
        flagged[0].triage = Some(crate::types::Triage {
            interest: 7.5,
            kind: "first_hand".into(),
            why: "specific field notes".into(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T05:30:00Z".parse().unwrap(),
        });
        flagged[0].article.excerpt_only = true;
        let prompt = build_prompt(&flagged, &sections(), 6, 11);
        assert!(prompt.contains("triage: 7.5 · first_hand — specific field notes"));
        assert!(prompt.contains("flags: always-include | exploration | excerpt only"));
    }

    #[tokio::test]
    async fn selection_builds_a_valid_lineup() {
        let backend = Arc::new(MockBackend::new());
        backend.push(LINEUP_FIXTURE, TokenUsage::default());
        let llms = bulk_only(Arc::clone(&backend));

        // ids 101..=112 so the fixture's picks resolve.
        let pool: Vec<ScoredArticle> = (101..=112)
            .map(|i| candidate(i, &format!("Article {i}"), 800, 7.0))
            .collect();
        let lineup = select(&llms, pool, &sections(), 6, 11, date())
            .await
            .expect("selection");

        assert_eq!(lineup.date, date());
        assert_eq!(lineup.picks.len(), 6);
        assert_eq!(lineup.picks.iter().filter(|p| p.is_lead).count(), 1);
        assert_eq!(lineup.lead().map(|p| p.article.id), Some(101));
        // Every section is from the palette and non-empty.
        for section in &lineup.section_order {
            assert!(sections().contains(section), "{section} is off-palette");
            assert!(!lineup.section_picks(section).is_empty());
        }
        // Positions restart at 1 inside each section and ascend.
        for section in &lineup.section_order {
            let positions: Vec<i64> = lineup
                .section_picks(section)
                .iter()
                .map(|p| p.position)
                .collect();
            assert_eq!(
                positions,
                (1..=positions.len() as i64).collect::<Vec<_>>(),
                "{section} positions"
            );
        }
        // The lead sits in the first section used.
        assert_eq!(
            lineup.lead().map(|p| p.section.clone()),
            lineup.section_order.first().cloned()
        );
        // The prompt carried the shortlist and the palette.
        let prompt = &backend.prompts()[0].user;
        assert!(prompt.starts_with("TASK: assemble today's issue of The Daily EPUB"));
        assert!(prompt.contains("--- id: 101"));
        assert!(prompt.contains("aim for about 6; never more than 11"));
    }

    #[tokio::test]
    async fn why_lines_land_on_picks() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(3), TokenUsage::default());
        let lineup = select(
            &bulk_only(backend),
            candidates(5),
            &sections(),
            3,
            5,
            date(),
        )
        .await
        .expect("selection");
        assert_eq!(lineup.picks.len(), 3);
        for pick in &lineup.picks {
            assert_eq!(
                pick.why.as_deref(),
                Some(format!("pick {} because", pick.article.id).as_str())
            );
        }
    }

    #[tokio::test]
    async fn hallucinated_ids_and_missing_leads_are_repaired() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"picks":[{"id":9999,"section":"Top Stories","position":1,"lead_story":true},
                         {"id":1,"section":"Sportsball","position":2},
                         {"id":2,"section":"Niche Corner","position":1},
                         {"id":2,"section":"Niche Corner","position":2}]}"#,
            TokenUsage::default(),
        );
        let lineup = select(
            &bulk_only(backend),
            candidates(6),
            &sections(),
            2,
            7,
            date(),
        )
        .await
        .expect("selection");
        assert!(lineup.picks.iter().all(|p| p.article.id != 9999));
        assert_eq!(lineup.picks.len(), 2, "the duplicate id was dropped");
        assert_eq!(lineup.picks.iter().filter(|p| p.is_lead).count(), 1);
        for pick in &lineup.picks {
            assert!(sections().contains(&pick.section));
        }
    }

    #[tokio::test]
    async fn a_nine_pick_answer_is_published_as_nine() {
        // Soft target 20, ceiling 28, thirty candidates: the model picks nine.
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(9), TokenUsage::default());
        let lineup = select(
            &bulk_only(backend),
            candidates(30),
            &sections(),
            20,
            28,
            date(),
        )
        .await
        .expect("selection");
        assert_eq!(lineup.picks.len(), 9, "no top-up, no padding");
    }

    #[tokio::test]
    async fn hard_max_trims_oversized_answers_by_ranking() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(30), TokenUsage::default());
        let lineup = select(
            &bulk_only(backend),
            candidates(30),
            &sections(),
            6,
            11,
            date(),
        )
        .await
        .expect("selection");
        assert_eq!(lineup.picks.len(), 11);
        // The strongest by today's ranking key survive: ids 1..=11 score highest.
        let mut ids: Vec<ArticleId> = lineup.picks.iter().map(|p| p.article.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=11).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn always_include_articles_are_reinserted_and_survive_the_trim() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(4), TokenUsage::default());
        let mut pool = candidates(30);
        pool[29].auto_include = true; // id 30, the weakest by score
        let lineup = select(&bulk_only(backend), pool, &sections(), 2, 4, date())
            .await
            .expect("selection");
        let ids: Vec<ArticleId> = lineup.picks.iter().map(|p| p.article.id).collect();
        assert!(ids.contains(&30), "auto-include must survive: {ids:?}");
        assert_eq!(lineup.picks.len(), 4, "the ceiling still holds");
        let reinserted = lineup
            .picks
            .iter()
            .find(|p| p.article.id == 30)
            .expect("reinserted");
        assert_eq!(reinserted.section, "From the Blogroll");
        assert!(reinserted.why.is_some());
    }

    #[tokio::test]
    async fn refusal_on_the_editor_falls_back_to_bulk_with_the_same_prompt() {
        let editor = Arc::new(MockBackend::new());
        editor.push_llm_error(LlmError::Refusal {
            provider: "anthropic",
        });
        let bulk = Arc::new(MockBackend::new());
        bulk.push(picks_json(5), TokenUsage::default());
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));

        let lineup = select(&llms, candidates(10), &sections(), 5, 10, date())
            .await
            .expect("selection");
        assert_eq!(lineup.picks.len(), 5);
        assert_eq!(editor.calls(), 1);
        assert_eq!(bulk.calls(), 1);
        assert_eq!(
            editor.prompts()[0].user,
            bulk.prompts()[0].user,
            "the bulk client gets the identical prompt"
        );
        assert_eq!(editor.prompts()[0].system, bulk.prompts()[0].system);
    }

    #[tokio::test]
    async fn an_error_on_both_providers_selects_heuristically() {
        let editor = Arc::new(MockBackend::new());
        editor.push_error("500 opus is down");
        let bulk = Arc::new(MockBackend::new());
        bulk.push_error("500 deepseek is down too");
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));
        let lineup = select(&llms, candidates(10), &sections(), 4, 10, date())
            .await
            .expect("heuristic fallback");
        assert_eq!(lineup.picks.len(), 4);
        assert_eq!(editor.calls(), 1);
        assert_eq!(bulk.calls(), 1);
    }

    #[tokio::test]
    async fn a_tripped_editor_budget_goes_straight_to_bulk() {
        let editor = Arc::new(MockBackend::new());
        let bulk = Arc::new(MockBackend::new());
        bulk.push(picks_json(3), TokenUsage::default());
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));
        llms.editor
            .as_ref()
            .expect("editor")
            .meter
            .preload_cost(10.0);
        let lineup = select(&llms, candidates(10), &sections(), 3, 10, date())
            .await
            .expect("selection");
        assert_eq!(lineup.picks.len(), 3);
        assert_eq!(editor.calls(), 0, "a tripped editor is never called");
        assert_eq!(bulk.calls(), 1);
    }

    #[tokio::test]
    async fn a_tripped_bulk_budget_falls_back_without_calling_the_model() {
        let backend = Arc::new(MockBackend::new());
        let llms = bulk_only(Arc::clone(&backend));
        llms.bulk.as_ref().expect("bulk").meter.record(TokenUsage {
            input_tokens: 100_000_000,
            cached_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
        });
        let lineup = select(&llms, candidates(20), &sections(), 6, 28, date())
            .await
            .expect("fallback");
        assert_eq!(backend.calls(), 0);
        assert_eq!(lineup.picks.len(), 6);
    }

    #[tokio::test]
    async fn no_provider_selects_heuristically() {
        let lineup = select(&Llms::default(), candidates(20), &sections(), 6, 28, date())
            .await
            .expect("fallback");
        assert_eq!(lineup.picks.len(), 6);
    }

    #[test]
    fn skip_llm_lineup_uses_preliminary_blend_order() {
        let mut pool = candidates(10);
        pool.iter_mut().for_each(|c| c.llm = None);
        pool[7].prefilter_score = 99.0; // id 8 is the strongest heuristically
        pool[9].auto_include = true; // id 10 is a personal blog
        pool[9].prefilter_score = 1.0;

        let lineup = select_without_llm(pool, &sections(), 4, 28, date());
        assert_eq!(lineup.picks.len(), 5, "4 picks + the auto-include");
        assert_eq!(lineup.lead().map(|p| p.article.id), Some(8));
        assert_eq!(lineup.picks.iter().filter(|p| p.is_lead).count(), 1);
        assert!(
            lineup
                .picks
                .iter()
                .any(|p| p.article.id == 10 && p.section == "From the Blogroll")
        );
        for pick in &lineup.picks {
            assert!(sections().contains(&pick.section));
            assert!(pick.summary.is_none());
            assert!(pick.why.is_none());
        }
        assert!(!lineup.section_order.is_empty());
    }

    #[test]
    fn heuristic_selection_respects_the_ceiling() {
        let mut pool = candidates(10);
        pool[9].auto_include = true;
        let lineup = select_without_llm(pool, &sections(), 10, 4, date());
        assert_eq!(lineup.picks.len(), 4);
        assert!(lineup.picks.iter().any(|p| p.article.id == 10));
    }

    #[test]
    fn empty_input_yields_an_empty_lineup() {
        let lineup = select_without_llm(Vec::new(), &sections(), 20, 28, date());
        assert!(lineup.picks.is_empty());
        assert!(lineup.section_order.is_empty());
        assert!(lineup.lead().is_none());
    }
}
