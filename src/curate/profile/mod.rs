//! Reader-profile and system-prompt construction (personalized curation v2 §8).
//!
//! Every run rebuilds one byte-stable prompt from the hand-maintained profile,
//! standing interests, stored weekly adjustments, and current explicit verdicts.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context as _;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::llm::LlmClient;
use crate::db::{Db, KV_PROFILE_VERSION, KV_TASTE_PROFILE};
use crate::types::{Facets, RatedArticle, TasteProfile};

pub const REBUILD_INTERVAL_DAYS: i64 = 7;
/// The verdict block and the weekly rebuild are bounded by count
/// (`verdicts_in_prompt`, [`MAX_RATINGS_IN_REBUILD`]), not by age (§8.3, §8.4),
/// so their `current_ratings` lookback is effectively unbounded.
const RATINGS_LOOKBACK_DAYS: i64 = 36_500;
pub const KV_LEARNED_ADJUSTMENTS: &str = "taste_profile_learned";
const MAX_RATINGS_IN_REBUILD: usize = 200;

pub const NO_LEARNED_ADJUSTMENTS: &str = "No reader ratings have been collected yet. Judge purely on the stated preferences and interests above.";

/// The only reader-profile prose that remains in code (§8.2).
const EDITOR_IN_CHIEF_FRAMING: &str = "You are the editor-in-chief of *The Daily EPUB*, a personal morning newspaper assembled every day for exactly one reader. Everything you are asked to do — score, select, place, summarize, introduce — serves his taste, not a general audience's. When a judgement call is close, re-read this profile and decide the way he would.";

// ---------------------------------------------------------------------------
// Interest and profile-file parsing
// ---------------------------------------------------------------------------

pub fn parse_interests(opml_path: &Path) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read_to_string(opml_path)
        .with_context(|| format!("reading the interests OPML at {}", opml_path.display()))?;
    let interests = parse_interests_str(&raw);
    if interests.is_empty() {
        anyhow::bail!(
            "no <outline text=\"…\"> interests found in {}",
            opml_path.display()
        );
    }
    tracing::debug!(count = interests.len(), "parsed scour interests");
    Ok(interests)
}

pub fn parse_interests_str(raw: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for chunk in raw.split("text=\"").skip(1) {
        let Some((value, _)) = chunk.split_once('"') else {
            continue;
        };
        let name = xml_unescape(value).trim().to_string();
        if !name.is_empty() && seen.insert(name.to_lowercase()) {
            out.push(name);
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileFile {
    /// Original Markdown with every `## Interests` section removed.
    pub body: String,
    pub interests: Vec<String>,
}

/// Remove any `## Interests` section and parse its non-empty lines as interests.
/// A leading `- ` is stripped; all other profile bytes pass through unchanged.
pub fn parse_profile_str(raw: &str) -> ProfileFile {
    let mut body = String::with_capacity(raw.len());
    let mut interests = Vec::new();
    let mut in_interests = false;

    for line in raw.split_inclusive('\n') {
        let heading = line.trim_end_matches(['\r', '\n']).trim();
        if heading.eq_ignore_ascii_case("## Interests") {
            in_interests = true;
            continue;
        }
        if in_interests && heading.starts_with("## ") {
            in_interests = false;
        }
        if in_interests {
            let interest = heading.strip_prefix("- ").unwrap_or(heading).trim();
            if !interest.is_empty()
                && !interest.eq_ignore_ascii_case(
                    "(optional: one per line; merged with data/scour-interests.opml)",
                )
            {
                interests.push(interest.to_string());
            }
        } else {
            body.push_str(line);
        }
    }
    ProfileFile { body, interests }
}

pub fn load_profile(path: &Path) -> anyhow::Result<ProfileFile> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(parse_profile_str(&raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "profile file is missing; using OPML interests only");
            Ok(ProfileFile {
                body: String::new(),
                interests: Vec::new(),
            })
        }
        Err(error) => {
            Err(error).with_context(|| format!("reading the reader profile at {}", path.display()))
        }
    }
}

/// Load the exact standing-interest union used in the system prompt.
pub fn load_standing_interests(
    opml_path: &Path,
    profile_path: &Path,
) -> anyhow::Result<Vec<String>> {
    let opml = parse_interests(opml_path)?;
    let profile = load_profile(profile_path)?;
    Ok(union_interests(opml, profile.interests))
}

fn union_interests(opml: Vec<String>, profile: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for interest in opml.into_iter().chain(profile) {
        let interest = interest.trim();
        if !interest.is_empty() && seen.insert(interest.to_lowercase()) {
            out.push(interest.to_string());
        }
    }
    out
}

pub mod themes;
pub use themes::group_into_themes;

// ---------------------------------------------------------------------------
// Prompt assembly
// ---------------------------------------------------------------------------

fn verdict_label(label: &str) -> &str {
    match label {
        "loved" => "LOVED",
        "good" => "GOOD",
        "not_for_me" => "NOT FOR ME",
        other => other,
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Assemble sections in the exact cache-friendly order required by §8.4.
pub fn build(
    profile_body: &str,
    interests: &[String],
    learned_adjustments: &str,
    ratings: &[RatedArticle],
    verdict_limit: usize,
) -> String {
    let mut doc = String::with_capacity(16 * 1024);
    doc.push_str(EDITOR_IN_CHIEF_FRAMING);
    doc.push_str("\n\n");

    if !profile_body.is_empty() {
        doc.push_str(profile_body);
        if !profile_body.ends_with('\n') {
            doc.push('\n');
        }
        doc.push('\n');
    }

    doc.push_str("## Standing interests\n\n");
    doc.push_str("These are his subscribed interest topics, grouped. They raise the floor for a match, but never cap the paper: an outstanding article on none of these still belongs.\n\n");
    for (theme, members) in group_into_themes(interests) {
        let _ = writeln!(doc, "- **{}**: {}", theme, members.join(", "));
    }

    doc.push_str("\n## Learned adjustments (rebuilt weekly from ratings)\n\n");
    let learned = learned_adjustments.trim();
    doc.push_str(if learned.is_empty() {
        NO_LEARNED_ADJUSTMENTS
    } else {
        learned
    });

    doc.push_str("\n\n## Recent verdicts\n\n");
    for rating in ratings.iter().take(verdict_limit) {
        let summary = rating
            .summary
            .as_deref()
            .map(one_line)
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| "no summary available".to_string());
        let feed = if rating.feed_title.trim().is_empty() {
            "unknown"
        } else {
            rating.feed_title.trim()
        };
        let _ = writeln!(
            doc,
            "{} | {} | {} | {}",
            verdict_label(&rating.label),
            one_line(&rating.title),
            one_line(feed),
            summary
        );
    }
    doc
}

// ---------------------------------------------------------------------------
// Persistence and per-run loading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileVersion {
    version: i64,
    built_at: String,
}

async fn stored_version(db: &Db) -> anyhow::Result<Option<(i64, Timestamp)>> {
    let Some(raw) = db.kv_get(KV_PROFILE_VERSION).await? else {
        return Ok(None);
    };
    match serde_json::from_str::<ProfileVersion>(&raw) {
        Ok(version) => {
            let built_at = version.built_at.parse::<Timestamp>().unwrap_or_else(|_| {
                tracing::warn!(value = %version.built_at, "unparseable profile build time");
                Timestamp::UNIX_EPOCH
            });
            Ok(Some((version.version, built_at)))
        }
        Err(error) => {
            tracing::warn!(%error, "unparseable kv[profile_version]; treating as absent");
            Ok(None)
        }
    }
}

async fn store_version(db: &Db, version: i64, built_at: Timestamp) -> anyhow::Result<()> {
    let json = serde_json::to_string(&ProfileVersion {
        version,
        built_at: built_at.to_string(),
    })?;
    db.kv_set(KV_PROFILE_VERSION, &json).await?;
    Ok(())
}

async fn prompt_inputs(
    db: &Db,
    opml_path: &Path,
    profile_path: &Path,
) -> anyhow::Result<(ProfileFile, Vec<String>, Vec<RatedArticle>, String)> {
    let opml = parse_interests(opml_path)?;
    let profile = load_profile(profile_path)?;
    let interests = union_interests(opml, profile.interests.clone());
    let ratings = db.current_ratings(RATINGS_LOOKBACK_DAYS).await?;
    let learned = db.kv_get(KV_LEARNED_ADJUSTMENTS).await?.unwrap_or_default();
    Ok((profile, interests, ratings, learned))
}

/// Rebuild the complete system prompt from its live inputs on every run.
pub async fn load_or_build(
    db: &Db,
    opml_path: &Path,
    profile_path: &Path,
    verdict_limit: usize,
) -> anyhow::Result<TasteProfile> {
    let (profile_file, interests, ratings, learned) =
        prompt_inputs(db, opml_path, profile_path).await?;
    let (version, built_at) = match stored_version(db).await? {
        Some(stored) => stored,
        None => {
            let built_at = Timestamp::now();
            store_version(db, 1, built_at).await?;
            (1, built_at)
        }
    };
    let profile = TasteProfile {
        text: build(
            &profile_file.body,
            &interests,
            &learned,
            &ratings,
            verdict_limit,
        ),
        version,
        built_at,
        verdicts: ratings.len().min(verdict_limit),
    };
    db.kv_set(KV_TASTE_PROFILE, &profile.text).await?;
    tracing::debug!(
        version,
        interests = interests.len(),
        verdicts = ratings.len().min(verdict_limit),
        chars = profile.text.len(),
        "rebuilt the taste profile prompt"
    );
    Ok(profile)
}

pub async fn is_stale(db: &Db) -> anyhow::Result<bool> {
    let Some((_, built_at)) = stored_version(db).await? else {
        return Ok(true);
    };
    let age_days = (Timestamp::now().as_second() - built_at.as_second()) / 86_400;
    Ok(age_days >= REBUILD_INTERVAL_DAYS)
}

pub async fn weekly_rebuild_if_due(
    db: &Db,
    llm: &LlmClient,
    opml_path: &Path,
    profile_path: &Path,
    verdict_limit: usize,
) -> anyhow::Result<Option<TasteProfile>> {
    if !is_stale(db).await? {
        return Ok(None);
    }
    if db.current_ratings(RATINGS_LOOKBACK_DAYS).await?.is_empty() {
        tracing::debug!("profile is stale but there are no ratings to learn from");
        return Ok(None);
    }
    tracing::info!("taste profile is over a week old; rebuilding learned adjustments");
    Ok(Some(
        rebuild(db, llm, opml_path, profile_path, verdict_limit).await?,
    ))
}

// ---------------------------------------------------------------------------
// Weekly learned-adjustments rebuild
// ---------------------------------------------------------------------------

pub const LEARNED_ADJUSTMENTS_PROMPT: &str = r#"TASK: rewrite the "Learned adjustments" section of the reader profile in your system prompt, using only the rating history below.

Each line is an explicit verdict with the article title, feed, summary, any deep-assessment facets, and the operator's note.

Look for patterns, not one-offs. Treat the stated preferences as a strong prior, not a rule. When repeated, recent behaviour clearly conflicts with an older stated preference, say so. Do not override a stated preference on one or two ratings.

Write 120–200 words as 4–8 bullet points, each one imperative and usable while scoring. One bullet is required and must begin "Diversity check:": name any subject or format that is starting to dominate the loved list and should not crowd out the rest of the paper. Do not mention specific article titles, rating counts, or this instruction. If the history is too thin to support any pattern, say so in one sentence instead of inventing one, while still including the Diversity check bullet.

Return JSON exactly: {"learned_adjustments": "<the bullet points, as markdown>"}

RATING HISTORY (newest first):
"#;

#[derive(Debug, Clone, Deserialize)]
struct LearnedAdjustmentsResponse {
    #[serde(default)]
    learned_adjustments: String,
}

fn facets_line(facets: Option<&Facets>) -> Option<String> {
    let facets = facets?;
    let values = [
        facets.format.as_deref(),
        facets.depth.as_deref(),
        facets.evidence.as_deref(),
        facets.technicality.as_deref(),
        facets.topic_group.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.trim().is_empty())
    .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join("/"))
}

pub fn build_rebuild_prompt(ratings: &[RatedArticle]) -> String {
    let mut prompt = String::from(LEARNED_ADJUSTMENTS_PROMPT);
    for rating in ratings.iter().take(MAX_RATINGS_IN_REBUILD) {
        let mut parts = vec![
            verdict_label(&rating.label).to_string(),
            one_line(&rating.title),
            if rating.feed_title.trim().is_empty() {
                "unknown".to_string()
            } else {
                one_line(&rating.feed_title)
            },
        ];
        if let Some(summary) = rating
            .summary
            .as_deref()
            .map(one_line)
            .filter(|s| !s.is_empty())
        {
            parts.push(summary);
        }
        if let Some(facets) = facets_line(rating.facets.as_ref()) {
            parts.push(format!("facets: {facets}"));
        }
        if let Some(note) = rating
            .note
            .as_deref()
            .map(one_line)
            .filter(|s| !s.is_empty())
        {
            parts.push(format!("note: {note}"));
        }
        let _ = writeln!(prompt, "{}", parts.join(" | "));
    }
    prompt
}

pub async fn rebuild(
    db: &Db,
    llm: &LlmClient,
    opml_path: &Path,
    profile_path: &Path,
    verdict_limit: usize,
) -> anyhow::Result<TasteProfile> {
    let ratings = db.current_ratings(RATINGS_LOOKBACK_DAYS).await?;
    let previous = db.kv_get(KV_LEARNED_ADJUSTMENTS).await?.unwrap_or_default();
    let learned = if ratings.is_empty() {
        tracing::info!("no ratings available; keeping the existing adjustments");
        previous
    } else {
        let prompt = build_rebuild_prompt(&ratings);
        match llm
            .complete_json::<LearnedAdjustmentsResponse>(&prompt, 0.4)
            .await
        {
            Ok(response) if !response.learned_adjustments.trim().is_empty() => {
                response.learned_adjustments.trim().to_string()
            }
            Ok(_) => {
                tracing::warn!("the model returned empty adjustments; keeping the previous ones");
                previous
            }
            Err(error) => {
                tracing::warn!(%error, "learned-adjustments rewrite failed; keeping the previous ones");
                previous
            }
        }
    };

    db.kv_set(KV_LEARNED_ADJUSTMENTS, &learned).await?;
    let next_version = stored_version(db)
        .await?
        .map_or(1, |(version, _)| version + 1);
    let built_at = Timestamp::now();
    store_version(db, next_version, built_at).await?;

    let opml = parse_interests(opml_path)?;
    let profile_file = load_profile(profile_path)?;
    let interests = union_interests(opml, profile_file.interests.clone());
    let current = db.current_ratings(RATINGS_LOOKBACK_DAYS).await?;
    let profile = TasteProfile {
        text: build(
            &profile_file.body,
            &interests,
            &learned,
            &current,
            verdict_limit,
        ),
        version: next_version,
        built_at,
        verdicts: current.len().min(verdict_limit),
    };
    db.kv_set(KV_TASTE_PROFILE, &profile.text).await?;
    tracing::info!(
        version = next_version,
        chars = profile.text.len(),
        "stored a new taste profile"
    );
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPML_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/scour-interests.opml");
    const PROFILE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/profile.md");

    #[test]
    fn profile_interests_are_removed_and_union_case_insensitively() {
        let parsed = parse_profile_str(
            "# P\n\n## Interests\n- Rust\nBoston Tech\n- rust\n\n## Notes\nKeep this.\n",
        );
        assert_eq!(parsed.body, "# P\n\n## Notes\nKeep this.\n");
        assert_eq!(parsed.interests, ["Rust", "Boston Tech", "rust"]);
        let union = union_interests(vec!["rust".into(), "E-Ink".into()], parsed.interests);
        assert_eq!(union, ["rust", "E-Ink", "Boston Tech"]);
    }

    #[test]
    fn prompt_sections_are_ordered_and_verdicts_have_required_labels() {
        let rating = RatedArticle {
            article_id: 1,
            issue_date: None,
            title: "A title".into(),
            feed_title: "A feed".into(),
            summary: Some("A summary\nwith whitespace.".into()),
            facets: None,
            note: None,
            value: -1.0,
            label: "not_for_me".into(),
            event_at: "2026-08-15T12:00:00Z".parse().unwrap(),
        };
        let prompt = build(
            "# Reader profile\n\nProfile prose.",
            &["Rust".into()],
            "- Adjust.",
            &[rating],
            60,
        );
        let framing = prompt.find("editor-in-chief").unwrap();
        let profile = prompt.find("# Reader profile").unwrap();
        let interests = prompt.find("## Standing interests").unwrap();
        let learned = prompt.find("## Learned adjustments").unwrap();
        let verdicts = prompt.find("## Recent verdicts").unwrap();
        assert!(
            framing < profile && profile < interests && interests < learned && learned < verdicts
        );
        assert!(prompt.contains("NOT FOR ME | A title | A feed | A summary with whitespace."));
    }

    #[test]
    fn shipped_profile_and_opml_parse() {
        let profile = load_profile(Path::new(PROFILE_PATH)).unwrap();
        assert!(profile.body.contains("## Who he is"));
        assert!(!profile.body.contains("## Interests"));
        assert!(profile.interests.is_empty());
        let interests = parse_interests(Path::new(OPML_PATH)).unwrap();
        assert!(interests.iter().any(|interest| interest == "Rust"));
    }

    #[test]
    fn rebuild_prompt_carries_summary_facets_note_and_diversity_instruction() {
        let rating = RatedArticle {
            article_id: 1,
            issue_date: Some("2026-08-15".parse().unwrap()),
            title: "Postgres failover".into(),
            feed_title: "Engineering Notes".into(),
            summary: Some("A detailed incident report.".into()),
            facets: Some(Facets {
                format: Some("first_hand_account".into()),
                depth: Some("deep".into()),
                evidence: Some("first_hand".into()),
                technicality: Some("advanced".into()),
                topic_group: Some("software_engineering".into()),
                ..Facets::default()
            }),
            note: Some("Great operational detail".into()),
            value: 1.0,
            label: "loved".into(),
            event_at: "2026-08-15T12:00:00Z".parse().unwrap(),
        };
        let prompt = build_rebuild_prompt(&[rating]);
        assert!(prompt.contains("strong prior, not a rule"));
        assert!(prompt.contains("Diversity check:"));
        assert!(prompt.contains(
            "LOVED | Postgres failover | Engineering Notes | A detailed incident report."
        ));
        assert!(
            prompt.contains(
                "facets: first_hand_account/deep/first_hand/advanced/software_engineering"
            )
        );
        assert!(prompt.contains("note: Great operational detail"));
    }

    #[tokio::test]
    async fn per_run_profile_reload_changes_prompt_without_bumping_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("profile.db"))
            .await
            .unwrap();
        let opml = dir.path().join("interests.opml");
        let profile_path = dir.path().join("profile.md");
        std::fs::write(&opml, r#"<outline text="Rust"/>"#).unwrap();
        std::fs::write(
            &profile_path,
            "# Reader profile\n\nOriginal prose.\n\n## Interests\n- Custom Topic\n",
        )
        .unwrap();

        let first = load_or_build(&db, &opml, &profile_path, 60).await.unwrap();
        assert_eq!(first.version, 1);
        assert!(first.text.contains("Original prose."));
        assert!(first.text.contains("Custom Topic"));
        assert!(!first.text.contains("## Interests"));

        std::fs::write(
            &profile_path,
            "# Reader profile\n\nChanged prose.\n\n## Interests\n- Another Topic\n",
        )
        .unwrap();
        let second = load_or_build(&db, &opml, &profile_path, 60).await.unwrap();
        assert_eq!(second.version, first.version);
        assert_eq!(second.built_at, first.built_at);
        assert!(second.text.contains("Changed prose."));
        assert!(second.text.contains("Another Topic"));
        assert!(!second.text.contains("Original prose."));

        let missing = load_profile(&dir.path().join("missing.md")).unwrap();
        assert!(missing.body.is_empty() && missing.interests.is_empty());
    }

    #[tokio::test]
    async fn weekly_rebuild_uses_mock_backend_and_bumps_profile_version() {
        use std::sync::Arc;

        use super::super::llm::{MockBackend, UsageMeter};
        use crate::config::ProviderConfig;
        use crate::types::{RatingEvent, TokenUsage};

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("profile.db"))
            .await
            .unwrap();
        let opml = dir.path().join("interests.opml");
        let profile_path = dir.path().join("profile.md");
        std::fs::write(&opml, r#"<outline text="Rust"/>"#).unwrap();
        std::fs::write(&profile_path, "# Reader profile\n\nLikes depth.\n").unwrap();
        let initial = load_or_build(&db, &opml, &profile_path, 60).await.unwrap();
        assert_eq!(initial.version, 1);

        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
             (1, 'https://example.com/deep', 'A deep report', '2026-08-15T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        db.append_rating_event(&RatingEvent {
            id: 0,
            article_id: 1,
            issue_date: None,
            kind: "explicit".into(),
            source: "cli".into(),
            label: "loved".into(),
            value: 1.0,
            note: Some("specific evidence".into()),
            event_at: Timestamp::now(),
        })
        .await
        .unwrap();

        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"learned_adjustments":"- Rank first-hand reports higher.\n- Diversity check: keep formats balanced."}"#,
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            initial.text,
            UsageMeter::for_provider(&ProviderConfig::deepseek()),
            backend.clone(),
        );
        let rebuilt = rebuild(&db, &llm, &opml, &profile_path, 60).await.unwrap();
        assert_eq!(rebuilt.version, 2);
        assert!(rebuilt.text.contains("Rank first-hand reports higher."));
        assert!(
            rebuilt
                .text
                .contains("Diversity check: keep formats balanced.")
        );
        assert_eq!(backend.calls(), 1);
        assert!(backend.prompts()[0].user.contains("LOVED | A deep report"));
    }
}
