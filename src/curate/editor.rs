//! Claude-first issue editor over the diversified shortlist (plan §13).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use super::llm::{LlmError, Llms, strip_code_fence};
use super::{prompt_text, truncate_words};
use crate::types::{ArticleId, Candidate, Facets, Lineup, Pick, WORLD_BRIEFING_SECTION};

const BLURB_WORDS: usize = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionItem {
    pub id: ArticleId,
    pub section: String,
    pub position: i64,
    #[serde(default)]
    pub lead_story: bool,
    #[serde(default)]
    pub why: Option<String>,
}

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

pub fn build_prompt(
    shortlist: &[Candidate],
    sections: &[String],
    soft_target: usize,
    hard_max: usize,
) -> String {
    let instructions = EDITOR_INSTRUCTIONS
        .replace("{soft_target}", &soft_target.to_string())
        .replace("{hard_max}", &hard_max.to_string());
    let mut prompt = String::with_capacity(2048 + shortlist.len() * 700);
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

fn render_candidate(candidate: &Candidate) -> String {
    let article = &candidate.article;
    let mut block = String::new();
    let _ = writeln!(block, "--- id: {}", article.id);
    let _ = writeln!(block, "title: {}", article.title.trim());
    let _ = writeln!(
        block,
        "feed: {} · {} words (~{} min)",
        if article.feed_title.trim().is_empty() {
            "unknown"
        } else {
            article.feed_title.trim()
        },
        article.word_count,
        article.reading_minutes()
    );
    let quality = candidate
        .assessment
        .deep
        .as_ref()
        .map(|deep| format!("{:.1}", deep.quality))
        .unwrap_or_else(|| "—".into());
    let fit = candidate
        .assessment
        .deep
        .as_ref()
        .map(|deep| format!("{:.1}", deep.fit))
        .unwrap_or_else(|| "—".into());
    let triage = candidate
        .assessment
        .triage
        .as_ref()
        .map(|triage| format!("{:.1}", triage.interest))
        .unwrap_or_else(|| "—".into());
    let rationale = candidate
        .assessment
        .deep
        .as_ref()
        .map(|deep| deep.rationale.trim())
        .filter(|rationale| !rationale.is_empty())
        .unwrap_or("no deep assessment");
    let _ = writeln!(
        block,
        "quality {quality} · fit {fit} · triage {triage} — {rationale}"
    );
    if let Some(facets) = candidate
        .assessment
        .deep
        .as_ref()
        .map(|deep| &deep.facets)
        .filter(|facets| **facets != Facets::default())
    {
        let _ = writeln!(
            block,
            "facets: {} · {} · {} · {} · {}",
            facets.format.as_deref().unwrap_or("unknown"),
            facets.depth.as_deref().unwrap_or("unknown"),
            facets.evidence.as_deref().unwrap_or("unknown"),
            facets.technicality.as_deref().unwrap_or("unknown"),
            facets.topic_group.as_deref().unwrap_or("unknown"),
        );
    }
    let interests = candidate
        .signals
        .top_interests
        .iter()
        .filter(|interest| interest.z >= 1.5)
        .map(|interest| {
            format!(
                "{} ({})",
                interest.name,
                if interest.z >= 2.5 { "strong" } else { "weak" }
            )
        })
        .collect::<Vec<_>>();
    if !interests.is_empty() {
        let _ = writeln!(block, "matches: {}", interests.join(", "));
    }
    let neighbours = candidate
        .signals
        .neighbours
        .iter()
        .filter(|neighbour| neighbour.cos >= 0.55)
        .map(|neighbour| {
            let label = match neighbour.label.as_str() {
                "loved" => "LOVED",
                "good" => "GOOD",
                "not_for_me" | "down" => "NOT FOR ME",
                other => other,
            };
            format!("{label} \"{}\" ({:.2})", neighbour.title, neighbour.cos)
        })
        .collect::<Vec<_>>();
    if !neighbours.is_empty() {
        let _ = writeln!(block, "closest rated: {}", neighbours.join("; "));
    }
    let mut flags = Vec::new();
    if candidate.exploration {
        flags.push("exploration");
    }
    if candidate.auto_include {
        flags.push("always-include");
    }
    if article.excerpt_only {
        flags.push("excerpt only");
    }
    if !flags.is_empty() {
        let _ = writeln!(block, "flags: {}", flags.join(" | "));
    }
    let opening = truncate_words(&prompt_text(&article.content_html), BLURB_WORDS);
    if !opening.is_empty() {
        let _ = writeln!(block, "opening: {opening}");
    }
    block
}

pub fn default_section(sections: &[String]) -> String {
    sections
        .iter()
        .find(|section| section.as_str() == "Top Stories")
        .or_else(|| sections.first())
        .cloned()
        .unwrap_or_else(|| "Top Stories".into())
}

pub fn resolve_section(raw: &str, sections: &[String]) -> String {
    let candidate = raw.trim();
    if candidate.is_empty() || candidate.eq_ignore_ascii_case(WORLD_BRIEFING_SECTION) {
        return default_section(sections);
    }
    if let Some(exact) = sections
        .iter()
        .find(|section| section.as_str() == candidate)
    {
        return exact.clone();
    }
    if let Some(case_insensitive) = sections
        .iter()
        .find(|section| section.eq_ignore_ascii_case(candidate))
    {
        return case_insensitive.clone();
    }
    let wanted = words_of(candidate);
    sections
        .iter()
        .map(|section| (section, words_of(section).intersection(&wanted).count()))
        .filter(|(_, overlap)| *overlap > 0)
        .max_by_key(|(_, overlap)| *overlap)
        .map(|(section, _)| section.clone())
        .unwrap_or_else(|| default_section(sections))
}

fn words_of(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.len() > 2 && !word.eq_ignore_ascii_case("and"))
        .map(str::to_lowercase)
        .collect()
}

pub fn heuristic_section(candidate: &Candidate, sections: &[String]) -> String {
    if candidate.auto_include {
        return resolve_section("From the Blogroll", sections);
    }
    if let Some(category) = candidate
        .assessment
        .deep
        .as_ref()
        .and_then(|deep| deep.category.as_deref())
    {
        return resolve_section(category, sections);
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
            ],
        ),
        (
            "Science & Space",
            &[
                "science", "space", "nasa", "astronom", "physics", "biology", "climate",
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
        if needles.iter().any(|needle| haystack.contains(needle))
            && let Some(found) = sections.iter().find(|value| value.as_str() == *section)
        {
            return found.clone();
        }
    }
    default_section(sections)
}

const ARRAY_KEYS: &[&str] = &["picks", "lineup", "articles", "selection", "items"];

pub fn parse_selection_response(raw: &str) -> Vec<SelectionItem> {
    let value: serde_json::Value = match serde_json::from_str(strip_code_fence(raw)) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "editor response was not JSON");
            return Vec::new();
        }
    };
    let array = match &value {
        serde_json::Value::Array(items) => Some(items),
        serde_json::Value::Object(map) => ARRAY_KEYS
            .iter()
            .find_map(|key| map.get(*key).and_then(serde_json::Value::as_array))
            .or_else(|| map.values().find_map(serde_json::Value::as_array)),
        _ => None,
    };
    array
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, item)| {
            let object = item.as_object()?;
            let id = object.get("id").and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })?;
            Some(SelectionItem {
                id,
                section: object
                    .get("section")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                position: object
                    .get("position")
                    .and_then(|value| {
                        value
                            .as_i64()
                            .or_else(|| value.as_str()?.trim().parse().ok())
                    })
                    .unwrap_or(index as i64 + 1),
                lead_story: object
                    .get("lead_story")
                    .or_else(|| object.get("is_lead"))
                    .and_then(|value| {
                        value.as_bool().or_else(|| {
                            value.as_str().map(|text| text.eq_ignore_ascii_case("true"))
                        })
                    })
                    .unwrap_or(false),
                why: object
                    .get("why")
                    .and_then(serde_json::Value::as_str)
                    .map(|why| {
                        why.split_whitespace()
                            .take(14)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .filter(|why| !why.is_empty()),
            })
        })
        .collect()
}

pub async fn select(
    llms: &Llms,
    candidates: Vec<Candidate>,
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
    let prompt = build_prompt(&candidates, sections, soft_target, hard_max);
    let raw = match complete_with_fallback(llms, primary, &prompt).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::error!(%error, "editor and bulk fallback both failed; selecting by utility");
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
        return Ok(select_without_llm(
            candidates,
            sections,
            soft_target,
            hard_max,
            date,
        ));
    }
    let by_id = candidates
        .iter()
        .map(|candidate| (candidate.article.id, candidate))
        .collect::<HashMap<_, _>>();
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if !seen.insert(item.id) {
            continue;
        }
        if let Some(candidate) = by_id.get(&item.id) {
            chosen.push((item, (*candidate).clone()));
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

const EDITOR_TEMPERATURE: f32 = 0.4;

async fn complete_with_fallback(
    llms: &Llms,
    primary: &super::llm::LlmClient,
    prompt: &str,
) -> Result<String, LlmError> {
    match primary.complete(prompt, EDITOR_TEMPERATURE, true).await {
        Ok(raw) => Ok(raw),
        Err(primary_error) => {
            let Some(fallback) = llms
                .bulk
                .as_ref()
                .filter(|bulk| primary.provider != bulk.provider)
            else {
                return Err(primary_error);
            };
            fallback.complete(prompt, EDITOR_TEMPERATURE, true).await
        }
    }
}

fn ordering_score(candidate: &Candidate) -> f64 {
    candidate
        .utility
        .or(candidate.signals.preliminary)
        .unwrap_or(f64::NEG_INFINITY)
}

fn assemble(
    mut chosen: Vec<(SelectionItem, Candidate)>,
    sections: &[String],
    hard_max: usize,
    date: Date,
) -> Lineup {
    if chosen.len() > hard_max {
        tracing::info!(
            picked = chosen.len(),
            hard_max,
            "editor exceeded the ceiling; trimming by utility"
        );
        chosen.sort_by(|left, right| {
            right
                .1
                .auto_include
                .cmp(&left.1.auto_include)
                .then_with(|| ordering_score(&right.1).total_cmp(&ordering_score(&left.1)))
                .then_with(|| left.1.article.id.cmp(&right.1.article.id))
        });
        chosen.truncate(hard_max);
    }
    for (item, _) in &mut chosen {
        item.section = resolve_section(&item.section, sections);
    }
    let used = chosen
        .iter()
        .map(|(item, _)| item.section.as_str())
        .collect::<HashSet<_>>();
    let mut section_order = sections
        .iter()
        .filter(|section| used.contains(section.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for (item, _) in &chosen {
        if !section_order.contains(&item.section) {
            section_order.push(item.section.clone());
        }
    }
    let section_rank = section_order
        .iter()
        .enumerate()
        .map(|(rank, section)| (section.as_str(), rank))
        .collect::<HashMap<_, _>>();
    chosen.sort_by(|left, right| {
        section_rank
            .get(left.0.section.as_str())
            .cmp(&section_rank.get(right.0.section.as_str()))
            .then_with(|| left.0.position.cmp(&right.0.position))
            .then_with(|| ordering_score(&right.1).total_cmp(&ordering_score(&left.1)))
            .then_with(|| left.1.article.id.cmp(&right.1.article.id))
    });
    let lead_id = chosen
        .iter()
        .find(|(item, _)| item.lead_story)
        .filter(|(item, _)| section_rank.get(item.section.as_str()) == Some(&0))
        .or_else(|| chosen.first())
        .map(|(item, _)| item.id);
    let mut per_section = BTreeMap::<String, i64>::new();
    let picks = chosen
        .into_iter()
        .map(|(item, candidate)| {
            let position = per_section
                .entry(item.section.clone())
                .and_modify(|value| *value += 1)
                .or_insert(1);
            Pick {
                article: candidate.article,
                section: item.section,
                position: *position,
                is_lead: Some(item.id) == lead_id,
                why: item.why,
                summary: None,
                llm: candidate.assessment.deep,
                discussion: None,
            }
        })
        .collect();
    Lineup {
        date,
        picks,
        section_order,
    }
}

pub fn select_without_llm(
    mut candidates: Vec<Candidate>,
    sections: &[String],
    soft_target: usize,
    hard_max: usize,
    date: Date,
) -> Lineup {
    candidates.sort_by(|left, right| {
        ordering_score(right)
            .total_cmp(&ordering_score(left))
            .then_with(|| left.article.id.cmp(&right.article.id))
    });
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
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
    use crate::config::{CurationConfig, ProviderConfig};
    use crate::curate::llm::{ChatBackend, LlmClient, MockBackend, PriceTable, UsageMeter};
    use crate::curate::prefilter::tests::article;
    use crate::curate::signals::{Neighbour, TopInterest};
    use crate::types::{Deep, TokenUsage, Triage};
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

    fn deep(quality: f64, fit: f64, rationale: &str) -> Deep {
        Deep {
            quality,
            fit,
            category: Some("Tech & Engineering".into()),
            rationale: rationale.into(),
            paywalled_guess: false,
            facets: Facets::default(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T05:30:00Z".parse().expect("timestamp"),
        }
    }

    /// A shortlisted candidate whose utility follows `score` (0–10).
    fn candidate(id: i64, title: &str, words: i64, score: f64) -> Candidate {
        let mut candidate = Candidate::new(article(id, title, words), false);
        candidate.assessment.deep = Some(deep(score, score, "solid"));
        candidate.utility = Some(score * 10.0);
        candidate.signals.preliminary = Some(40.0 + score);
        candidate.stage = "shortlisted".into();
        candidate
    }

    fn candidates(n: i64) -> Vec<Candidate> {
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
        // The reserved section is never allowed through.
        assert_eq!(resolve_section(WORLD_BRIEFING_SECTION, &s), "Top Stories");
        // A palette without "Top Stories" falls back to its first entry.
        let tiny = vec!["Niche Corner".to_string()];
        assert_eq!(resolve_section("Whatever", &tiny), "Niche Corner");
    }

    #[test]
    fn heuristic_sections_prefer_the_deep_category_then_feed_metadata() {
        let s = sections();
        let mut c = candidate(1, "MBTA slow zones, charted", 900, 6.0);
        c.assessment.deep = None;
        c.article.category = Some("News".into());
        assert_eq!(heuristic_section(&c, &s), "Boston & Local");

        let mut assessed = candidate(2, "MBTA slow zones, charted", 900, 6.0);
        assessed.article.category = Some("News".into());
        assessed.assessment.deep = Some(Deep {
            category: Some("Boston & Local".into()),
            ..deep(6.0, 6.0, "charted")
        });
        assert_eq!(heuristic_section(&assessed, &s), "Boston & Local");

        let mut ai = candidate(3, "A new LLM benchmark", 900, 6.0);
        ai.assessment.deep = None;
        ai.article.category = Some("Machine Learning".into());
        assert_eq!(heuristic_section(&ai, &s), "AI & Machine Learning");

        let mut blog = candidate(4, "Notes from my week", 900, 6.0);
        blog.auto_include = true;
        assert_eq!(heuristic_section(&blog, &s), "From the Blogroll");

        let mut plain = candidate(5, "Untitled musing", 900, 6.0);
        plain.assessment.deep = None;
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
    fn the_prompt_substitutes_the_size_targets_and_renders_the_shortlist() {
        let prompt = build_prompt(&candidates(3), &sections(), 6, 11);
        assert!(prompt.contains("aim for about 6; never more than 11; there is NO minimum"));
        assert!(!prompt.contains("{soft_target}") && !prompt.contains("{hard_max}"));
        assert!(prompt.contains("SHORTLIST (3 candidates, best-ranked first)"));
        assert!(prompt.contains("--- id: 1\n"));
        assert!(prompt.contains("feed: Some Blog · 510 words (~"));
        assert!(prompt.contains("quality 9.9 · fit 9.9 · triage — — solid"));
        assert!(prompt.contains("opening: word word"));
        assert!(
            !prompt.contains("utility") && !prompt.contains("99.0"),
            "the numeric blend stays out of the prompt"
        );
        assert!(
            !prompt.contains("facets:"),
            "no facets line when every facet is unknown"
        );
    }

    #[test]
    fn prompt_renders_deep_facets_matches_neighbours_and_flags() {
        let mut candidate = candidate(1, "A field report", 1_850, 8.5);
        candidate.exploration = true;
        candidate.auto_include = true;
        candidate.article.excerpt_only = true;
        candidate.assessment.triage = Some(Triage {
            interest: 8.0,
            kind: "essay".into(),
            why: "promising".into(),
            model: "mock".into(),
            prompt_version: 1,
            assessed_at: "2026-09-02T00:00:00Z".parse().expect("timestamp"),
        });
        candidate.assessment.deep = Some(Deep {
            facets: Facets {
                format: Some("first_hand_account".into()),
                depth: Some("deep".into()),
                evidence: Some("first_hand".into()),
                technicality: Some("advanced".into()),
                topic_group: Some("software_engineering".into()),
                ..Facets::default()
            },
            ..deep(8.5, 7.0, "Measured field report")
        });
        candidate.signals.top_interests = vec![
            TopInterest {
                name: "Gaussian Splatting".into(),
                z: 3.4,
                cos: 0.61,
            },
            TopInterest {
                name: "Science".into(),
                z: 0.4,
                cos: 0.3,
            },
        ];
        candidate.signals.neighbours = vec![Neighbour {
            article_id: 812,
            label: "loved".into(),
            cos: 0.71,
            title: "The failover story".into(),
        }];
        let prompt = build_prompt(&[candidate], &sections(), 20, 28);
        assert!(prompt.contains("feed: Some Blog · 1850 words (~"));
        assert!(prompt.contains("quality 8.5 · fit 7.0 · triage 8.0 — Measured field report"));
        assert!(prompt.contains(
            "facets: first_hand_account · deep · first_hand · advanced · software_engineering"
        ));
        assert!(prompt.contains("matches: Gaussian Splatting (strong)\n"));
        assert!(prompt.contains("closest rated: LOVED \"The failover story\" (0.71)"));
        assert!(prompt.contains("flags: exploration | always-include | excerpt only"));
        assert!(prompt.contains("opening: word word"));
        let opening = prompt
            .lines()
            .find(|line| line.starts_with("opening:"))
            .expect("opening line");
        assert!(opening.split_whitespace().count() <= BLURB_WORDS + 2);
    }

    #[tokio::test]
    async fn selection_builds_a_valid_lineup() {
        let backend = Arc::new(MockBackend::new());
        backend.push(LINEUP_FIXTURE, TokenUsage::default());
        let llms = bulk_only(Arc::clone(&backend));

        // ids 101..=112 so the fixture's picks resolve.
        let pool: Vec<Candidate> = (101..=112)
            .map(|i| candidate(i, &format!("Article {i}"), 800, 7.0))
            .collect();
        let lineup = select(&llms, pool, &sections(), 6, 11, date())
            .await
            .expect("selection");

        assert_eq!(lineup.date, date());
        assert_eq!(lineup.picks.len(), 6);
        assert_eq!(lineup.picks.iter().filter(|p| p.is_lead).count(), 1);
        assert_eq!(lineup.lead().map(|p| p.article.id), Some(101));
        for section in &lineup.section_order {
            assert!(sections().contains(section), "{section} is off-palette");
            assert!(!lineup.section_picks(section).is_empty());
        }
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
        assert_eq!(
            lineup.lead().map(|p| p.section.clone()),
            lineup.section_order.first().cloned()
        );
        // Picks carry their deep assessment for the editorial stage.
        assert!(lineup.picks.iter().all(|p| p.llm.is_some()));
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
    async fn hard_max_trims_oversized_answers_by_utility() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(30), TokenUsage::default());
        // Utility, not the deep scores or the blend, decides who survives:
        // id 30 has the weakest quality but the strongest utility.
        let mut pool = candidates(30);
        pool[29].utility = Some(200.0);
        let lineup = select(&bulk_only(backend), pool, &sections(), 6, 11, date())
            .await
            .expect("selection");
        assert_eq!(lineup.picks.len(), 11);
        let mut ids: Vec<ArticleId> = lineup.picks.iter().map(|p| p.article.id).collect();
        ids.sort_unstable();
        let mut expected: Vec<ArticleId> = (1..=10).collect();
        expected.push(30);
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn hard_max_trim_falls_back_to_the_preliminary_blend_without_utility() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(6), TokenUsage::default());
        let mut pool = candidates(6);
        for candidate in &mut pool {
            candidate.utility = None;
        }
        pool[5].signals.preliminary = Some(99.0);
        let lineup = select(&bulk_only(backend), pool, &sections(), 2, 3, date())
            .await
            .expect("selection");
        let mut ids: Vec<ArticleId> = lineup.picks.iter().map(|p| p.article.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 6]);
    }

    #[tokio::test]
    async fn always_include_articles_are_reinserted_and_survive_the_trim() {
        let backend = Arc::new(MockBackend::new());
        backend.push(picks_json(4), TokenUsage::default());
        let mut pool = candidates(30);
        pool[29].auto_include = true; // id 30, the weakest by utility
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
        editor.push_llm_error(LlmError::refusal("anthropic"));
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
    async fn an_error_on_both_providers_selects_by_utility() {
        let editor = Arc::new(MockBackend::new());
        editor.push_error("500 opus is down");
        let bulk = Arc::new(MockBackend::new());
        bulk.push_error("500 deepseek is down too");
        let llms = editor_and_bulk(Arc::clone(&editor), Arc::clone(&bulk));
        let mut pool = candidates(10);
        pool[9].utility = Some(150.0);
        let lineup = select(&llms, pool, &sections(), 4, 10, date())
            .await
            .expect("heuristic fallback");
        assert_eq!(lineup.picks.len(), 4);
        assert_eq!(lineup.lead().map(|p| p.article.id), Some(10));
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
    async fn no_provider_selects_by_utility() {
        let lineup = select(&Llms::default(), candidates(20), &sections(), 6, 28, date())
            .await
            .expect("fallback");
        assert_eq!(lineup.picks.len(), 6);
        assert_eq!(lineup.lead().map(|p| p.article.id), Some(1));
    }

    #[test]
    fn select_without_llm_orders_by_utility() {
        let mut pool = candidates(10);
        pool[7].utility = Some(150.0); // id 8 is the strongest by utility
        pool[7].signals.preliminary = Some(1.0); // ...despite the weakest blend
        pool[9].auto_include = true; // id 10 is a personal blog
        pool[9].utility = Some(1.0);

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
    fn select_without_llm_falls_back_to_the_preliminary_blend() {
        let mut pool = candidates(4);
        for candidate in &mut pool {
            candidate.utility = None;
            candidate.assessment.deep = None;
        }
        pool[2].signals.preliminary = Some(99.0);
        let lineup = select_without_llm(pool, &sections(), 2, 4, date());
        assert_eq!(lineup.picks.len(), 2);
        assert_eq!(lineup.lead().map(|pick| pick.article.id), Some(3));
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
