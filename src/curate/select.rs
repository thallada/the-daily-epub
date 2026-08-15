//! Stage B — lineup selection (spec §3.6).
//!
//! One call: send the top ~40 candidates by [`ScoredArticle::combined_score`] with
//! their rationales; the model returns the final 15–25 picks, each with a section
//! from the configured palette, an ordering, and exactly one `lead_story`.
//!
//! The model's answer is treated as a proposal, never as gospel: sections are
//! validated against the palette, the lead is forced to be unique, auto-include
//! feeds are re-inserted if they were dropped, and the size is clamped to
//! `target_article_count ± 5`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use super::llm::{LlmClient, LlmError, strip_code_fence};
use super::{html_to_text, truncate_words};
use crate::types::{ArticleId, Lineup, Pick, ScoredArticle, SourceKind, WORLD_BRIEFING_SECTION};

/// How many candidates are offered to stage B (§3.6).
pub const SHORTLIST_SIZE: usize = 40;
/// How far the final count may drift from `target_article_count` (§3.6: 15–25
/// around a default target of 20).
pub const TARGET_TOLERANCE: usize = 5;
/// Words of lead-in text shown per candidate in the stage-B prompt.
const BLURB_WORDS: usize = 45;

/// One element of the stage-B JSON response (§3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionItem {
    pub id: ArticleId,
    /// Must be one of `curation.sections`.
    pub section: String,
    pub position: i64,
    #[serde(default)]
    pub lead_story: bool,
}

/// Envelope the model is asked to return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionResponse {
    #[serde(default)]
    pub picks: Vec<SelectionItem>,
}

/// The invariant instruction block for stage B (§3.6).
pub const SELECT_INSTRUCTIONS: &str = "\
TASK: assemble today's issue of The Daily EPUB from the shortlist below.

You are choosing what one specific reader — the profile in your system prompt — \
will actually read on an e-ink screen over breakfast. Build a paper, not a \
ranking: it should have a shape, a range of subjects, and a clear front page.

RULES
1. Pick articles by id from the shortlist only. Never invent an id.
2. Give every pick a section from the palette below, spelled exactly as given.
3. Number picks within each section from 1 upward, best first.
4. Flag exactly one pick as \"lead_story\": true — the day's strongest, most \
substantial piece. It must sit in the first section you use.
5. Any candidate marked \"always-include\" MUST appear; place it in \"From the \
Blogroll\" unless it clearly belongs elsewhere.
6. Do not select two articles that tell the same story; keep the better one.

EDITORIAL JUDGEMENT
- Favour depth over coverage: a slim issue of excellent pieces beats a full one \
padded with filler. Drop anything you would not defend.
- Mix the day up. Several long technical dives in a row is a bad breakfast; \
alternate register and subject across sections.
- Keep the local and ultra-niche picks — a Boston story and a small-scene story \
are worth more here than a third AI-industry item.
- Score is evidence, not an instruction: overrule it when the paper reads better \
for it, and say so through your placement.
- Leave a section out entirely rather than padding it; empty sections are dropped.

Return JSON exactly in this shape and nothing else:
{\"picks\": [{\"id\": 123, \"section\": \"Top Stories\", \"position\": 1, \
\"lead_story\": true}]}";

/// Render the stage-B user prompt (§3.6).
pub fn build_prompt(shortlist: &[ScoredArticle], sections: &[String], target: usize) -> String {
    let (min, max) = size_bounds(target);
    let mut prompt = String::with_capacity(2048 + shortlist.len() * 400);
    prompt.push_str(SELECT_INSTRUCTIONS);
    let _ = write!(
        prompt,
        "\n\nSECTION PALETTE (exact strings, use only these): {}\n\
         Reserved and unavailable: \"{WORLD_BRIEFING_SECTION}\" is compiled separately.\n\n\
         SIZE: choose {target} articles; never fewer than {min} and never more than {max}.\n\n\
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
                "score: {:.1} ({}) — {}",
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
            let _ = writeln!(
                block,
                "score: unscored (heuristic rank {:.0}/100)",
                candidate.prefilter_score
            );
        }
    }
    let _ = writeln!(
        block,
        "signals: social {:.2}; feed prior {:.2}; via {}{}",
        candidate.social_score,
        candidate.feed_prior,
        source_kinds(candidate),
        if candidate.auto_include {
            "; ALWAYS-INCLUDE"
        } else {
            ""
        }
    );
    let blurb = truncate_words(&html_to_text(&a.content_html), BLURB_WORDS);
    if !blurb.is_empty() {
        let _ = writeln!(block, "opening: {blurb}");
    }
    block
}

fn source_kinds(candidate: &ScoredArticle) -> String {
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
    if kinds.is_empty() {
        "feed".into()
    } else {
        kinds.join("+")
    }
}

/// `target ± TARGET_TOLERANCE`, floored at one article (§3.6).
pub fn size_bounds(target: usize) -> (usize, usize) {
    let target = target.max(1);
    (
        target.saturating_sub(TARGET_TOLERANCE).max(1),
        target + TARGET_TOLERANCE,
    )
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

/// Section guess from feed metadata, used by `--skip-llm` and by top-ups (§3.6).
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

/// Lenient parse of the stage-B response (§3.6).
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
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Stage driver
// ---------------------------------------------------------------------------

/// Ask the model for the day's lineup, validating that every section is from the
/// configured palette and exactly one pick is the lead (§3.6).
pub async fn select(
    llm: &LlmClient,
    candidates: Vec<ScoredArticle>,
    sections: &[String],
    target: usize,
    date: Date,
) -> Result<Lineup, LlmError> {
    if candidates.is_empty() {
        tracing::warn!("stage B had no candidates");
        return Ok(Lineup {
            date,
            picks: Vec::new(),
            section_order: Vec::new(),
        });
    }
    if let Err(e) = llm.meter.check_budget() {
        tracing::error!(error = %e,
            "COST CEILING HIT before stage B selection — falling back to heuristic ranking");
        return Ok(select_without_llm(candidates, sections, target, date));
    }

    let shortlist = shortlist(&candidates, target);
    let prompt = build_prompt(&shortlist, sections, target);
    tracing::debug!(
        shortlist = shortlist.len(),
        approx_tokens = super::approx_tokens(&prompt),
        "stage B request"
    );

    let raw = llm.complete(&prompt, llm_temperature(), true).await?;
    let items = parse_selection_response(&raw);
    if items.is_empty() {
        tracing::error!("stage B returned no usable picks; falling back to heuristic ranking");
        return Ok(select_without_llm(candidates, sections, target, date));
    }

    let by_id: HashMap<ArticleId, &ScoredArticle> =
        candidates.iter().map(|c| (c.article.id, c)).collect();
    let mut chosen: Vec<(SelectionItem, ScoredArticle)> = Vec::with_capacity(items.len());
    let mut seen: HashSet<ArticleId> = HashSet::new();
    for item in items {
        if !seen.insert(item.id) {
            tracing::warn!(id = item.id, "stage B picked the same article twice");
            continue;
        }
        match by_id.get(&item.id) {
            Some(candidate) => chosen.push((item, (*candidate).clone())),
            None => tracing::warn!(id = item.id, "stage B invented an id that was not offered"),
        }
    }

    // Auto-include feeds can never be dropped (§3.5).
    for candidate in &candidates {
        if candidate.auto_include && seen.insert(candidate.article.id) {
            tracing::info!(
                id = candidate.article.id,
                title = %candidate.article.title,
                "re-inserting an always-include article the model dropped"
            );
            chosen.push((
                SelectionItem {
                    id: candidate.article.id,
                    section: "From the Blogroll".into(),
                    position: i64::MAX,
                    lead_story: false,
                },
                candidate.clone(),
            ));
        }
    }

    let lineup = assemble(chosen, &candidates, sections, target, date);
    tracing::info!(
        picks = lineup.picks.len(),
        sections = lineup.section_order.len(),
        lead = lineup.lead().map(|p| p.article.id),
        "stage B lineup ready"
    );
    Ok(lineup)
}

/// Stage B runs at the scoring temperature: this is a judgement call, not prose.
fn llm_temperature() -> f32 {
    0.4
}

/// Top [`SHORTLIST_SIZE`] candidates by combined score, always including the
/// auto-includes (§3.6).
fn shortlist(candidates: &[ScoredArticle], target: usize) -> Vec<ScoredArticle> {
    let mut ranked: Vec<ScoredArticle> = candidates.to_vec();
    sort_by_combined(&mut ranked);
    let keep = SHORTLIST_SIZE.max(target * 2);
    if ranked.len() <= keep {
        return ranked;
    }
    let (head, tail) = ranked.split_at(keep);
    let mut out = head.to_vec();
    out.extend(tail.iter().filter(|c| c.auto_include).cloned());
    out
}

fn sort_by_combined(candidates: &mut [ScoredArticle]) {
    candidates.sort_by(|a, b| {
        b.combined_score()
            .partial_cmp(&a.combined_score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.article.id.cmp(&b.article.id))
    });
}

/// Turn validated picks into a [`Lineup`]: clamp the size, force a single lead,
/// order the sections and renumber positions (§3.6).
fn assemble(
    mut chosen: Vec<(SelectionItem, ScoredArticle)>,
    all: &[ScoredArticle],
    sections: &[String],
    target: usize,
    date: Date,
) -> Lineup {
    let (min, max) = size_bounds(target);

    // Too many: drop the weakest non-auto-include picks.
    if chosen.len() > max {
        chosen.sort_by(|a, b| {
            b.1.auto_include.cmp(&a.1.auto_include).then_with(|| {
                b.1.combined_score()
                    .partial_cmp(&a.1.combined_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let dropped = chosen.len() - max;
        chosen.truncate(max);
        tracing::info!(dropped, max, "trimmed the lineup to the size ceiling");
    }

    // Too few: top up from the best unpicked candidates.
    if chosen.len() < min {
        let taken: HashSet<ArticleId> = chosen.iter().map(|(i, _)| i.id).collect();
        let mut rest: Vec<ScoredArticle> = all
            .iter()
            .filter(|c| !taken.contains(&c.article.id))
            .cloned()
            .collect();
        sort_by_combined(&mut rest);
        let wanted = min - chosen.len();
        let added = rest.len().min(wanted);
        for candidate in rest.into_iter().take(wanted) {
            let section = heuristic_section(&candidate, sections);
            chosen.push((
                SelectionItem {
                    id: candidate.article.id,
                    section,
                    position: i64::MAX,
                    lead_story: false,
                },
                candidate,
            ));
        }
        tracing::info!(added, min, "topped the lineup up to the size floor");
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

/// `--skip-llm` fallback: take the top `target` by prefilter score and bucket them
/// into sections by feed category (notes §6).
pub fn select_without_llm(
    candidates: Vec<ScoredArticle>,
    sections: &[String],
    target: usize,
    date: Date,
) -> Lineup {
    let mut ranked = candidates.clone();
    super::prefilter::sort_by_prefilter(&mut ranked);

    let mut chosen: Vec<(SelectionItem, ScoredArticle)> = Vec::new();
    let mut seen: HashSet<ArticleId> = HashSet::new();
    for candidate in ranked.into_iter() {
        let auto = candidate.auto_include;
        if chosen.len() >= target && !auto {
            continue;
        }
        if !seen.insert(candidate.article.id) {
            continue;
        }
        let section = heuristic_section(&candidate, sections);
        chosen.push((
            SelectionItem {
                id: candidate.article.id,
                section,
                position: chosen.len() as i64 + 1,
                lead_story: false,
            },
            candidate,
        ));
    }
    let lineup = assemble(chosen, &candidates, sections, target, date);
    tracing::info!(
        picks = lineup.picks.len(),
        sections = lineup.section_order.len(),
        "skip-llm lineup ready"
    );
    lineup
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CurationConfig, DeepseekConfig};
    use crate::curate::llm::{MockBackend, UsageMeter};
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
            feed_prior: 0.5,
            llm: Some(LlmScore {
                score,
                category: "Tech & Engineering".into(),
                rationale: "solid".into(),
                is_paywalled_guess: false,
            }),
            auto_include: false,
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
        // Junk entries in the fixture are dropped, not fatal.
        assert!(items.iter().all(|i| i.id != 0));
    }

    #[test]
    fn size_bounds_follow_the_spec() {
        assert_eq!(size_bounds(20), (15, 25));
        assert_eq!(size_bounds(6), (1, 11));
        assert_eq!(size_bounds(0), (1, 6));
    }

    #[tokio::test]
    async fn selection_builds_a_valid_lineup() {
        let backend = Arc::new(MockBackend::new());
        backend.push(LINEUP_FIXTURE, TokenUsage::default());
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend.clone(),
        );

        // ids 101..=112 so the fixture's picks resolve.
        let pool: Vec<ScoredArticle> = (101..=112)
            .map(|i| candidate(i, &format!("Article {i}"), 800, 7.0))
            .collect();
        let lineup = select(&llm, pool, &sections(), 6, date())
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
        assert!(prompt.starts_with(SELECT_INSTRUCTIONS));
        assert!(prompt.contains("--- id: 101"));
        assert!(prompt.contains("never fewer than 1 and never more than 11"));
    }

    #[tokio::test]
    async fn hallucinated_ids_and_missing_leads_are_repaired() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"picks":[{"id":9999,"section":"Top Stories","position":1,"lead_story":true},
                         {"id":1,"section":"Sportsball","position":2},
                         {"id":2,"section":"Niche Corner","position":1}]}"#,
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend,
        );
        let lineup = select(&llm, candidates(6), &sections(), 2, date())
            .await
            .expect("selection");
        assert!(lineup.picks.iter().all(|p| p.article.id != 9999));
        assert_eq!(lineup.picks.iter().filter(|p| p.is_lead).count(), 1);
        for pick in &lineup.picks {
            assert!(sections().contains(&pick.section));
        }
    }

    #[tokio::test]
    async fn oversized_and_undersized_answers_are_clamped() {
        // Undersized: the model returns one pick but the floor is 5.
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"picks":[{"id":1,"section":"Top Stories","position":1,"lead_story":true}]}"#,
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend,
        );
        let lineup = select(&llm, candidates(30), &sections(), 10, date())
            .await
            .expect("selection");
        assert!(lineup.picks.len() >= 5, "{}", lineup.picks.len());

        // Oversized: 30 picks against a target of 6 (ceiling 11).
        let picks: Vec<String> = (1..=30)
            .map(|i| format!(r#"{{"id":{i},"section":"Top Stories","position":{i}}}"#))
            .collect();
        let backend = Arc::new(MockBackend::new());
        backend.push(
            format!(r#"{{"picks":[{}]}}"#, picks.join(",")),
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend,
        );
        let lineup = select(&llm, candidates(30), &sections(), 6, date())
            .await
            .expect("selection");
        assert_eq!(lineup.picks.len(), 11);
    }

    #[tokio::test]
    async fn always_include_articles_are_reinserted() {
        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"picks":[{"id":1,"section":"Top Stories","position":1,"lead_story":true}]}"#,
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend,
        );
        let mut pool = candidates(3);
        pool[2].auto_include = true;
        let lineup = select(&llm, pool, &sections(), 1, date())
            .await
            .expect("selection");
        let ids: Vec<ArticleId> = lineup.picks.iter().map(|p| p.article.id).collect();
        assert!(ids.contains(&3), "auto-include must survive: {ids:?}");
        assert_eq!(
            lineup
                .picks
                .iter()
                .find(|p| p.article.id == 3)
                .map(|p| p.section.as_str()),
            Some("From the Blogroll")
        );
    }

    #[tokio::test]
    async fn a_tripped_budget_falls_back_without_calling_the_model() {
        let backend = Arc::new(MockBackend::new());
        let meter = UsageMeter::new(&DeepseekConfig::default(), 0.001);
        meter.record(TokenUsage {
            input_tokens: 1_000_000,
            cached_tokens: 0,
            output_tokens: 0,
        });
        let llm =
            LlmClient::with_backend("deepseek-v4-flash", "SYSTEM".into(), meter, backend.clone());
        let lineup = select(&llm, candidates(20), &sections(), 6, date())
            .await
            .expect("fallback");
        assert_eq!(backend.calls(), 0);
        assert_eq!(lineup.picks.len(), 6);
    }

    #[test]
    fn skip_llm_lineup_uses_prefilter_order() {
        let mut pool = candidates(10);
        pool.iter_mut().for_each(|c| c.llm = None);
        pool[7].prefilter_score = 99.0; // id 8 is the strongest heuristically
        pool[9].auto_include = true; // id 10 is a personal blog
        pool[9].prefilter_score = 1.0;

        let lineup = select_without_llm(pool, &sections(), 4, date());
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
        }
        assert!(!lineup.section_order.is_empty());
    }

    #[test]
    fn empty_input_yields_an_empty_lineup() {
        let lineup = select_without_llm(Vec::new(), &sections(), 20, date());
        assert!(lineup.picks.is_empty());
        assert!(lineup.section_order.is_empty());
        assert!(lineup.lead().is_none());
    }
}
