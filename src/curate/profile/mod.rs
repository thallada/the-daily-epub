//! Taste profile construction (spec §3.6, §3.9).
//!
//! A ~600-word document assembled from (a) the interest names parsed out of
//! `data/scour-interests.opml`, grouped into themes, (b) hard-coded stated
//! preferences, and (c) a "learned adjustments" section regenerated weekly from
//! recent 👍/👎 ratings. Stored and versioned in `kv`.
//!
//! This document is the **system prompt** for every DeepSeek call in the run, so
//! it must be byte-identical between requests: DeepSeek's automatic prefix cache
//! is what makes the whole pipeline cost cents rather than dollars (§3.6).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context as _;
use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use sqlx::Row as _;

use super::llm::LlmClient;
use crate::db::{Db, KV_PROFILE_VERSION, KV_TASTE_PROFILE};
use crate::types::{TasteProfile, Vote};

/// Rebuild cadence for the learned-adjustments section (§3.6).
pub const REBUILD_INTERVAL_DAYS: i64 = 7;
/// Ratings lookback used when rewriting learned adjustments (§3.9).
pub const RATINGS_LOOKBACK_DAYS: i64 = 90;
/// `kv` key holding just the learned-adjustments block, so that re-parsing the
/// OPML never loses what the ratings taught us (§3.6).
pub const KV_LEARNED_ADJUSTMENTS: &str = "taste_profile_learned";
/// Ratings fed to one rebuild call.
const MAX_RATINGS_IN_PROMPT: usize = 400;

/// Hard-coded stated preferences from the reader profile (spec §1).
pub const STATED_PREFERENCES: &str = "\
Prefers long-form, high-effort, well-written articles on any topic. Uses social \
proof (HN/Reddit/Lobsters upvotes and comment counts) as a quality proxy. Wants \
tech news, light general/US world news (Wikipedia Current Events style, neutral), \
Boston-area news, and ultra-niche community news.";

/// Placeholder used until the first ratings arrive (§3.6c).
pub const NO_LEARNED_ADJUSTMENTS: &str = "No reader ratings have been collected yet. Judge purely on the stated \
     preferences and interests above.";

// ---------------------------------------------------------------------------
// OPML parsing (§3.6a)
// ---------------------------------------------------------------------------

/// Parse interest names out of the Scour OPML (§3.6).
///
/// The file is one long line of `<outline type="rss" text="Rust" …/>` elements;
/// we take every `text` attribute, XML-unescape it, trim it, and de-duplicate
/// case-insensitively (the export contains both `Self-hosting` and
/// `Self-Hosting`). Order follows the document so the result is deterministic.
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

/// [`parse_interests`] over an in-memory document (also the unit-test seam).
pub fn parse_interests_str(raw: &str) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for chunk in raw.split("text=\"").skip(1) {
        let Some((value, _)) = chunk.split_once('"') else {
            continue;
        };
        let name = xml_unescape(value).trim().to_string();
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
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

pub mod themes;

pub use themes::group_into_themes;

// ---------------------------------------------------------------------------
// Document assembly (§3.6)
// ---------------------------------------------------------------------------

/// The invariant part of the profile: who the reader is and how to judge for him.
/// Kept as one constant so the prompt bytes never drift between calls (§3.6).
const PROFILE_PREAMBLE: &str = "\
You are the editor-in-chief of *The Daily EPUB*, a personal morning newspaper \
assembled every day for exactly one reader. Everything you are asked to do — \
score, select, place, summarize, introduce — serves his taste, not a general \
audience's. When a judgement call is close, re-read this profile and decide the \
way he would.

## The reader

A software engineer in the Boston area who reads on e-ink in the morning. He \
would rather read six excellent long pieces than thirty adequate short ones. He \
reads across an unusually wide range of subjects and does not need a topic to be \
professionally useful to enjoy it.

## What he wants

- **Long-form and high-effort above all.** Essays, deep dives, post-mortems, \
field notes, annotated experiments, thorough explainers, personal narratives with \
real specificity. Length is a proxy, not the goal: what he is buying is evident \
effort and a point of view.
- **Any topic, if the writing is excellent.** A brilliant piece on medieval \
bookbinding beats a competent one on his favourite language. Do not reject \
something merely because it sits outside the interest list below.
- **Social proof as a quality signal, not a ranking.** Hundreds of HN or Reddit \
points and a busy comment thread mean the piece survived contact with a critical \
audience — treat it as evidence, then judge the writing yourself. A quiet post \
from a good blog can outrank a viral one.
- **Boston and New England local news** — city government, transit, universities, \
neighbourhood and civic stories.
- **Ultra-niche community news.** Small scenes with their own vocabulary — a \
mailing-list argument, a hobby project's release story, a subculture's internal \
debate — are a feature of this paper, not a distraction.
- **World and US news kept light and neutral.** Wikipedia-Current-Events register: \
what happened, who is involved, no outrage, no opinion columns. The World \
Briefing section is compiled separately; do not fill the paper with wire copy.

## What he does not want

Press releases and funding announcements dressed as news; SEO listicles; \
link-roundup and \"this week in X\" posts; changelogs and release notes without \
analysis; sponsored content and thinly disguised marketing; crypto and \
engagement-bait; rewrites of a story he can read at the source; culture-war \
outrage; anything whose substance is one paragraph stretched to five.

## How to judge

Ask: *would he still be glad he read this an hour later?* Reward specificity, \
first-hand experience, honest uncertainty, and prose with a human behind it. \
Penalize padding, unsourced confidence, and summaries of other people's work. \
Prefer the primary source over the aggregator when both are present.";

/// Assemble the full profile document from interests, stated preferences and the
/// current learned-adjustments block (§3.6).
///
/// Pure and deterministic: the same inputs always produce the same bytes.
pub fn build(interests: &[String], learned_adjustments: &str) -> String {
    let mut doc = String::with_capacity(8 * 1024);
    doc.push_str("# The Daily EPUB — reader taste profile\n\n");
    doc.push_str(PROFILE_PREAMBLE);
    doc.push_str("\n\n## Stated preferences (verbatim)\n\n");
    doc.push_str(STATED_PREFERENCES);
    doc.push_str("\n\n## Standing interests\n\n");
    doc.push_str(
        "These are his ~220 subscribed interest topics, grouped. They raise the \
         floor for a match, but never cap the paper: an outstanding article on \
         none of these still belongs.\n\n",
    );
    for (theme, members) in group_into_themes(interests) {
        let _ = writeln!(doc, "- **{}**: {}", theme, members.join(", "));
    }
    doc.push_str("\n## Learned adjustments (rebuilt weekly from 👍/👎 ratings)\n\n");
    let learned = learned_adjustments.trim();
    doc.push_str(if learned.is_empty() {
        NO_LEARNED_ADJUSTMENTS
    } else {
        learned
    });
    doc.push('\n');
    doc
}

// ---------------------------------------------------------------------------
// Persistence (§3.6 `kv`)
// ---------------------------------------------------------------------------

/// `kv[profile_version]` payload.
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
        Ok(v) => {
            let built = v.built_at.parse::<Timestamp>().unwrap_or_else(|_| {
                tracing::warn!(value = %v.built_at, "unparseable profile build time");
                Timestamp::UNIX_EPOCH
            });
            Ok(Some((v.version, built)))
        }
        Err(e) => {
            tracing::warn!(error = %e, "unparseable kv[profile_version]; treating as absent");
            Ok(None)
        }
    }
}

async fn store(db: &Db, profile: &TasteProfile, learned: &str) -> anyhow::Result<()> {
    db.kv_set(KV_TASTE_PROFILE, &profile.text).await?;
    db.kv_set(KV_LEARNED_ADJUSTMENTS, learned).await?;
    let version = serde_json::to_string(&ProfileVersion {
        version: profile.version,
        built_at: profile.built_at.to_string(),
    })?;
    db.kv_set(KV_PROFILE_VERSION, &version).await?;
    Ok(())
}

/// Load the stored profile, building a default one on first run (§3.6).
pub async fn load_or_build(db: &Db, opml_path: &Path) -> anyhow::Result<TasteProfile> {
    if let Some(text) = db.kv_get(KV_TASTE_PROFILE).await?.filter(|t| !t.is_empty()) {
        let (version, built_at) = stored_version(db).await?.unwrap_or((1, Timestamp::now()));
        tracing::debug!(version, chars = text.len(), "loaded stored taste profile");
        return Ok(TasteProfile {
            text,
            version,
            built_at,
        });
    }
    let interests = parse_interests(opml_path)?;
    let learned = db.kv_get(KV_LEARNED_ADJUSTMENTS).await?.unwrap_or_default();
    let profile = TasteProfile {
        text: build(&interests, &learned),
        version: 1,
        built_at: Timestamp::now(),
    };
    store(db, &profile, &learned).await?;
    tracing::info!(
        interests = interests.len(),
        chars = profile.text.len(),
        "built the initial taste profile"
    );
    Ok(profile)
}

/// True when the stored profile is older than [`REBUILD_INTERVAL_DAYS`] (§3.6).
pub async fn is_stale(db: &Db) -> anyhow::Result<bool> {
    let Some((_, built_at)) = stored_version(db).await? else {
        return Ok(true);
    };
    let age_days = (Timestamp::now().as_second() - built_at.as_second()) / 86_400;
    Ok(age_days >= REBUILD_INTERVAL_DAYS)
}

/// The automatic weekly rebuild the pipeline calls before curating (§3.6).
///
/// Rebuilds only when the stored profile is at least a week old *and* there are
/// ratings to learn from; otherwise returns the profile unchanged. Failures are
/// non-fatal — a stale profile still curates fine.
pub async fn weekly_rebuild_if_due(
    db: &Db,
    llm: &LlmClient,
    opml_path: &Path,
) -> anyhow::Result<Option<TasteProfile>> {
    if !is_stale(db).await? {
        return Ok(None);
    }
    if recent_ratings(db).await?.is_empty() {
        tracing::debug!("profile is stale but there are no ratings to learn from");
        return Ok(None);
    }
    tracing::info!("taste profile is over a week old; rebuilding learned adjustments");
    Ok(Some(rebuild(db, llm, opml_path).await?))
}

// ---------------------------------------------------------------------------
// Rebuild (§3.6c, §3.9)
// ---------------------------------------------------------------------------

/// A rated article as fed to the learned-adjustments prompt (§3.6c).
#[derive(Debug, Clone, PartialEq)]
pub struct RatedArticle {
    pub vote: Vote,
    pub title: String,
    pub feed_title: String,
    pub category: String,
    /// The stage-A category the model itself assigned, when we have one.
    pub llm_category: String,
}

/// Recent ratings joined to article titles, feeds and categories (§3.6c).
///
/// `db.rs` exposes `recent_ratings_detailed`, but it returns titles only; the
/// prompt is much more useful with the feed and category attached.
pub async fn recent_ratings(db: &Db) -> anyhow::Result<Vec<RatedArticle>> {
    // Timestamp arithmetic only accepts uniform units, so days become hours.
    let since = Timestamp::now()
        .checked_sub(jiff::Span::new().hours(RATINGS_LOOKBACK_DAYS * 24))
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let since_date = since.to_zoned(jiff::tz::TimeZone::UTC).date();
    let rows = sqlx::query(
        "SELECT r.vote AS vote,
                COALESCE(a.title, '') AS title,
                COALESCE(e.feed_title, '') AS feed_title,
                COALESCE(e.category, '') AS category,
                COALESCE((SELECT s.llm_category FROM scores s
                           WHERE s.article_id = a.id AND s.llm_category IS NOT NULL
                           ORDER BY s.run_date DESC LIMIT 1), '') AS llm_category
           FROM ratings r
           JOIN articles a ON a.id = r.article_id
           LEFT JOIN entries e ON e.id = a.best_entry_id
          WHERE r.issue_date >= ?
          ORDER BY r.rated_at DESC
          LIMIT ?",
    )
    .bind(since_date.to_string())
    .bind(MAX_RATINGS_IN_PROMPT as i64)
    .fetch_all(db.pool())
    .await
    .context("loading recent ratings for the profile rebuild")?;

    Ok(rows
        .iter()
        .map(|r| RatedArticle {
            vote: if r.get::<i64, _>("vote") >= 0 {
                Vote::Up
            } else {
                Vote::Down
            },
            title: r.get("title"),
            feed_title: r.get("feed_title"),
            category: r.get("category"),
            llm_category: r.get("llm_category"),
        })
        .collect())
}

/// Instruction block for the weekly learned-adjustments rewrite (§3.6c).
pub const LEARNED_ADJUSTMENTS_PROMPT: &str = "\
TASK: rewrite the \"Learned adjustments\" section of the reader profile in your \
system prompt, using only the rating history below.

Each line is a thumbs-up or thumbs-down the reader gave an article that appeared \
in a past issue, with the article's title, the feed it came from, and its \
category.

Look for patterns, not one-offs. Good adjustments name a *kind* of article and a \
*reason*: \"consistently downvotes vendor engineering-blog posts that are really \
product announcements\"; \"consistently upvotes database-internals deep dives, \
even very long ones\"; \"lukewarm on AI-industry news, warm on hands-on LLM \
tinkering\". Ignore patterns supported by fewer than two ratings, and never \
contradict the stated preferences — refine them.

Write 120–200 words as 4–8 bullet points, each one imperative and usable while \
scoring (\"Rank X higher\", \"Be sceptical of Y\"). Do not mention specific \
article titles, the rating counts, or this instruction. If the history is too \
thin to support any pattern, say so in one sentence instead of inventing one.

Return JSON exactly: {\"learned_adjustments\": \"<the bullet points, as markdown>\"}

RATING HISTORY (newest first):
";

/// The weekly rewrite's JSON envelope.
#[derive(Debug, Clone, Deserialize)]
struct LearnedAdjustmentsResponse {
    #[serde(default)]
    learned_adjustments: String,
}

/// Render the rating history block of the rebuild prompt (§3.6c).
pub fn build_rebuild_prompt(ratings: &[RatedArticle]) -> String {
    let mut prompt = String::from(LEARNED_ADJUSTMENTS_PROMPT);
    let (mut up, mut down) = (0usize, 0usize);
    for r in ratings {
        match r.vote {
            Vote::Up => up += 1,
            Vote::Down => down += 1,
        }
        let category = if r.llm_category.is_empty() {
            r.category.as_str()
        } else {
            r.llm_category.as_str()
        };
        let _ = writeln!(
            prompt,
            "{} | {} | feed: {} | category: {}",
            match r.vote {
                Vote::Up => "UP  ",
                Vote::Down => "DOWN",
            },
            r.title.trim(),
            if r.feed_title.is_empty() {
                "unknown"
            } else {
                r.feed_title.trim()
            },
            if category.is_empty() {
                "unknown"
            } else {
                category.trim()
            },
        );
    }
    let _ = write!(prompt, "\n({up} up, {down} down)\n");
    prompt
}

/// `daily-epub profile rebuild` — summarize recent ratings into a new learned
/// adjustments section and store a new profile version (§3.6, §3.9).
pub async fn rebuild(db: &Db, llm: &LlmClient, opml_path: &Path) -> anyhow::Result<TasteProfile> {
    let interests = parse_interests(opml_path)?;
    let ratings = recent_ratings(db).await?;
    let previous = db.kv_get(KV_LEARNED_ADJUSTMENTS).await?.unwrap_or_default();

    let learned = if ratings.is_empty() {
        tracing::info!("no ratings in the lookback window; keeping the existing adjustments");
        previous
    } else {
        let prompt = build_rebuild_prompt(&ratings);
        match llm
            .complete_json::<LearnedAdjustmentsResponse>(&prompt, 0.4)
            .await
        {
            Ok(resp) if !resp.learned_adjustments.trim().is_empty() => {
                tracing::info!(
                    ratings = ratings.len(),
                    chars = resp.learned_adjustments.len(),
                    "rewrote the learned-adjustments section"
                );
                resp.learned_adjustments.trim().to_string()
            }
            Ok(_) => {
                tracing::warn!("the model returned empty adjustments; keeping the previous ones");
                previous
            }
            Err(e) => {
                tracing::warn!(error = %e, "learned-adjustments rewrite failed; keeping the previous ones");
                previous
            }
        }
    };

    let next_version = stored_version(db).await?.map_or(1, |(v, _)| v + 1);
    let profile = TasteProfile {
        text: build(&interests, &learned),
        version: next_version,
        built_at: Timestamp::now(),
    };
    store(db, &profile, &learned).await?;
    tracing::info!(
        version = profile.version,
        chars = profile.text.len(),
        "stored a new taste profile"
    );
    Ok(profile)
}

// ---------------------------------------------------------------------------
// Feed priors (§3.9a)
// ---------------------------------------------------------------------------

/// Recompute per-feed beta-smoothed priors from the ratings table (§3.9).
///
/// Returns the number of feeds written. `FeedPrior::rate()` does the smoothing;
/// this only maintains the raw counts plus how often the feed has been included.
pub async fn rebuild_feed_priors(db: &Db) -> anyhow::Result<usize> {
    use std::collections::HashMap;

    use crate::types::{FeedId, FeedPrior};

    let mut priors: HashMap<FeedId, FeedPrior> = HashMap::new();
    for (feed_id, vote) in db.ratings_with_feed().await? {
        let entry = priors.entry(feed_id).or_insert(FeedPrior {
            feed_id,
            ..FeedPrior::default()
        });
        match vote {
            Vote::Up => entry.upvotes += 1,
            Vote::Down => entry.downvotes += 1,
        }
    }

    let rows = sqlx::query(
        "SELECT e.feed_id AS feed_id, COUNT(*) AS included
           FROM issue_articles ia
           JOIN articles a ON a.id = ia.article_id
           JOIN entries e ON e.id = a.best_entry_id
          GROUP BY e.feed_id",
    )
    .fetch_all(db.pool())
    .await
    .context("counting per-feed inclusions")?;
    for row in &rows {
        let feed_id: FeedId = row.get("feed_id");
        let included: i64 = row.get("included");
        priors
            .entry(feed_id)
            .or_insert(FeedPrior {
                feed_id,
                ..FeedPrior::default()
            })
            .included = included;
    }

    for prior in priors.values() {
        db.upsert_feed_prior(prior).await?;
    }
    tracing::info!(feeds = priors.len(), "rebuilt feed priors");
    Ok(priors.len())
}

/// Convenience for callers that only have a [`Date`]: the ratings lookback start.
pub fn ratings_since(today: Date) -> Date {
    today
        .checked_sub(jiff::Span::new().days(RATINGS_LOOKBACK_DAYS))
        .unwrap_or(today)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Rating;

    const OPML_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/scour-interests.opml");

    fn interests() -> Vec<String> {
        parse_interests(Path::new(OPML_PATH)).expect("the shipped OPML parses")
    }

    #[test]
    fn parses_the_shipped_opml() {
        let list = interests();
        assert!(
            list.len() > 180,
            "expected ~220 interests, got {}",
            list.len()
        );
        assert!(list.iter().any(|i| i == "Rust"));
        assert!(list.iter().any(|i| i == "Boston Tech"));
        assert!(list.iter().any(|i| i == "E-Ink Displays"));
        // Trailing whitespace is trimmed and case-duplicates collapse.
        assert!(list.iter().any(|i| i == "photography"));
        let lowered: Vec<String> = list.iter().map(|i| i.to_lowercase()).collect();
        let unique: BTreeSet<&String> = lowered.iter().collect();
        assert_eq!(unique.len(), lowered.len(), "duplicates survived");
        assert!(!list.iter().any(|i| i.contains("scour.ing")));
    }

    #[test]
    fn parsing_handles_entities_and_empties() {
        let raw = r#"<opml><body>
            <outline text="Tea &amp; Coffee" xmlUrl="x?a=1&amp;b=2"/>
            <outline text="  Rust  "/>
            <outline text="rust"/>
            <outline text=""/>
        </body></opml>"#;
        assert_eq!(
            parse_interests_str(raw),
            vec!["Tea & Coffee".to_string(), "Rust".to_string()]
        );
    }

    #[test]
    fn profile_document_is_deterministic_and_complete() {
        let list = interests();
        let one = build(&list, "");
        let two = build(&list, "");
        assert_eq!(one, two, "profile assembly must be byte-stable");
        assert_eq!(one.as_bytes(), two.as_bytes());

        assert!(one.contains(STATED_PREFERENCES));
        assert!(one.contains(NO_LEARNED_ADJUSTMENTS));
        assert!(one.contains("Boston"));
        assert!(one.contains("Wikipedia-Current-Events"));
        assert!(one.contains("Rust"));
        assert!(one.contains("ultra-niche") || one.contains("Ultra-niche"));
        // A real document, not a stub, but not a novel either.
        let words = one.split_whitespace().count();
        assert!((500..3000).contains(&words), "profile is {words} words");

        let learned = build(&list, "- Rank database internals higher.");
        assert!(learned.contains("- Rank database internals higher."));
        assert!(!learned.contains(NO_LEARNED_ADJUSTMENTS));
    }

    async fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_and_migrate(&dir.path().join("profile.db"))
            .await
            .expect("db");
        (dir, db)
    }

    #[tokio::test]
    async fn load_or_build_persists_and_reuses() {
        let (_dir, db) = temp_db().await;
        let first = load_or_build(&db, Path::new(OPML_PATH))
            .await
            .expect("first build");
        assert_eq!(first.version, 1);
        assert!(!is_stale(&db).await.expect("staleness"));

        let second = load_or_build(&db, Path::new(OPML_PATH))
            .await
            .expect("second load");
        assert_eq!(first.text, second.text);
        assert_eq!(second.version, 1);
        assert_eq!(
            db.kv_get(KV_TASTE_PROFILE).await.expect("kv").as_deref(),
            Some(first.text.as_str())
        );
    }

    #[tokio::test]
    async fn missing_version_row_means_stale() {
        let (_dir, db) = temp_db().await;
        assert!(is_stale(&db).await.expect("staleness"));
        db.kv_set(KV_PROFILE_VERSION, "not json")
            .await
            .expect("kv set");
        assert!(is_stale(&db).await.expect("staleness"));
        db.kv_set(
            KV_PROFILE_VERSION,
            r#"{"version":3,"built_at":"2000-01-01T00:00:00Z"}"#,
        )
        .await
        .expect("kv set");
        assert!(is_stale(&db).await.expect("staleness"));
    }

    #[tokio::test]
    async fn rebuild_uses_the_model_and_bumps_the_version() {
        use super::super::llm::{MockBackend, UsageMeter};
        use crate::config::DeepseekConfig;
        use std::sync::Arc;

        let (_dir, db) = temp_db().await;
        load_or_build(&db, Path::new(OPML_PATH))
            .await
            .expect("initial");
        seed_rating(&db, 1, "Postgres index internals", Vote::Up).await;
        seed_rating(&db, 2, "Series B funding announced", Vote::Down).await;

        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"learned_adjustments": "- Rank database internals deep dives higher.\n- Be sceptical of funding announcements."}"#,
            crate::types::TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend.clone(),
        );

        let rebuilt = rebuild(&db, &llm, Path::new(OPML_PATH))
            .await
            .expect("rebuild");
        assert_eq!(rebuilt.version, 2);
        assert!(
            rebuilt
                .text
                .contains("Rank database internals deep dives higher.")
        );
        assert!(
            rebuilt
                .text
                .contains("Be sceptical of funding announcements.")
        );

        // The rating history reached the prompt, with feed + category context.
        let prompts = backend.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].user.contains("Postgres index internals"));
        assert!(
            prompts[0]
                .user
                .contains("DOWN | Series B funding announced")
        );
        assert!(prompts[0].user.contains("(1 up, 1 down)"));

        // A later reload sees the new document.
        let loaded = load_or_build(&db, Path::new(OPML_PATH))
            .await
            .expect("reload");
        assert_eq!(loaded.text, rebuilt.text);
        assert_eq!(loaded.version, 2);
    }

    #[tokio::test]
    async fn rebuild_without_ratings_skips_the_model() {
        use super::super::llm::{MockBackend, UsageMeter};
        use crate::config::DeepseekConfig;
        use std::sync::Arc;

        let (_dir, db) = temp_db().await;
        let backend = Arc::new(MockBackend::new());
        let llm = LlmClient::with_backend(
            "deepseek-v4-flash",
            "SYSTEM".into(),
            UsageMeter::new(&DeepseekConfig::default(), 2.0),
            backend.clone(),
        );
        let profile = rebuild(&db, &llm, Path::new(OPML_PATH))
            .await
            .expect("rebuild");
        assert_eq!(backend.calls(), 0, "no ratings ⇒ no LLM call");
        assert!(profile.text.contains(NO_LEARNED_ADJUSTMENTS));
        assert!(
            weekly_rebuild_if_due(&db, &llm, Path::new(OPML_PATH))
                .await
                .expect("weekly")
                .is_none()
        );
    }

    #[tokio::test]
    async fn feed_priors_are_recomputed_from_ratings() {
        let (_dir, db) = temp_db().await;
        seed_rating(&db, 1, "Good one", Vote::Up).await;
        seed_rating(&db, 2, "Bad one", Vote::Down).await;
        let feeds = rebuild_feed_priors(&db).await.expect("priors");
        assert_eq!(feeds, 1);
        let priors = db.feed_priors().await.expect("load");
        assert_eq!(priors.len(), 1);
        assert_eq!(priors[0].upvotes, 1);
        assert_eq!(priors[0].downvotes, 1);
        assert!((priors[0].rate() - 0.5).abs() < 1e-12);
    }

    /// Insert an entry + article + rating triple that the joins can see.
    async fn seed_rating(db: &Db, id: i64, title: &str, vote: Vote) {
        use crate::types::{Entry, ExtractMethod, SourceRef};
        let ts: Timestamp = "2026-08-15T05:30:00Z".parse().expect("ts");
        db.upsert_entry(&Entry {
            id,
            feed_id: 7,
            feed_title: Some("A Feed".into()),
            category: Some("Tech".into()),
            title: title.into(),
            url: format!("https://example.com/{id}"),
            canonical_url: Some(format!("https://example.com/{id}")),
            author: None,
            published_at: Some(ts),
            comments_url: None,
            raw_content: String::new(),
            fetched_at: ts,
        })
        .await
        .expect("entry");
        let article_id = db
            .upsert_article(&crate::types::Article {
                id: 0,
                canonical_url: format!("https://example.com/{id}"),
                title: title.into(),
                best_entry_id: id,
                content_html: String::new(),
                word_count: 900,
                excerpt_only: false,
                image_count: 0,
                sources: Vec::<SourceRef>::new(),
                first_seen: ts,
                url: format!("https://example.com/{id}"),
                author: None,
                feed_id: 7,
                feed_title: "A Feed".into(),
                category: Some("Tech".into()),
                published_at: Some(ts),
                comments_url: None,
                image_urls: vec![],
                social: vec![],
                extract_method: ExtractMethod::Miniflux,
            })
            .await
            .expect("article");
        db.upsert_rating(&Rating {
            issue_date: Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date(),
            article_id,
            vote,
            rated_at: Timestamp::now(),
        })
        .await
        .expect("rating");
    }
}
