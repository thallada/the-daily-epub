//! Shared domain types — the contract between pipeline stages (spec §2, §3.13).
//!
//! Every stage module (`dedupe`, `extract`, `social`, `curate`, `comments`, `epub`,
//! `publish`, `server`, `world`) codes against the types defined here so that the
//! stages can be implemented independently. Keep this module free of I/O.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};

/// Miniflux entry id (also our `entries.id`).
pub type EntryId = i64;
/// Row id of a deduped article cluster (`articles.id`).
pub type ArticleId = i64;
/// Miniflux feed id.
pub type FeedId = i64;

// ---------------------------------------------------------------------------
// Ingest (§3.1)
// ---------------------------------------------------------------------------

/// A raw Miniflux entry as persisted in the `entries` table (§3.1, §3.13).
///
/// `canonical_url` is `None` at ingest time; the dedupe stage
/// ([`crate::dedupe::canonical_url`], §3.2) fills it in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub feed_id: FeedId,
    pub feed_title: Option<String>,
    pub category: Option<String>,
    pub title: String,
    pub url: String,
    pub canonical_url: Option<String>,
    pub author: Option<String>,
    pub published_at: Option<Timestamp>,
    pub comments_url: Option<String>,
    pub raw_content: String,
    pub fetched_at: Timestamp,
}

/// Where an article reached us from — a curation signal in its own right (§3.2, §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Arrived via a Scour interest feed — it already matched a stated interest.
    Scour,
    /// Arrived via the Hacker News frontpage feed (hnrss et al).
    HnFrontpage,
    /// Arrived via a lobste.rs feed.
    Lobsters,
    /// Arrived via a Reddit feed.
    Reddit,
    /// A plain blog/publication feed.
    Feed,
}

/// One feed that carried this story; an article cluster keeps the union of them (§3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub entry_id: EntryId,
    pub feed_id: FeedId,
    pub feed_title: String,
    pub category: Option<String>,
    pub kind: SourceKind,
}

// ---------------------------------------------------------------------------
// Dedupe + extraction (§3.2, §3.3)
// ---------------------------------------------------------------------------

/// How an article's body text was obtained (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractMethod {
    /// Miniflux's stored content already looked like full text.
    Miniflux,
    /// Fetched the article URL and ran readability over it.
    Readability,
    /// Only a feed summary/excerpt was available.
    Excerpt,
}

/// Result of the content-extraction stage for one article (§3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extracted {
    /// Sanitized XHTML-safe body markup.
    pub content_html: String,
    pub word_count: i64,
    /// True when we only have an excerpt/paywall stub — penalized in pre-filter.
    pub excerpt_only: bool,
    /// Absolute image URLs referenced by the body, capped at 12 (§3.3).
    pub image_urls: Vec<String>,
    pub method: ExtractMethod,
}

/// A deduped story cluster: the unit everything downstream operates on (§3.2).
///
/// Persisted fields map to the `articles` table; the remaining fields are
/// denormalized from the best entry / `social` table for convenience and are
/// re-hydrated by [`crate::db`] when an article is loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Article {
    /// Zero until the row has been inserted.
    pub id: ArticleId,
    pub canonical_url: String,
    pub title: String,
    /// The entry whose content we kept (the richest one).
    pub best_entry_id: EntryId,
    pub content_html: String,
    pub word_count: i64,
    pub excerpt_only: bool,
    pub image_count: i64,
    /// Union of the feeds that carried this story (`articles.sources_json`).
    pub sources: Vec<SourceRef>,
    pub first_seen: Timestamp,

    // --- denormalized, not stored on `articles` ---
    pub url: String,
    pub author: Option<String>,
    pub feed_id: FeedId,
    pub feed_title: String,
    pub category: Option<String>,
    pub published_at: Option<Timestamp>,
    pub comments_url: Option<String>,
    pub image_urls: Vec<String>,
    pub social: Vec<SocialRef>,
    pub extract_method: ExtractMethod,
}

impl Article {
    /// Estimated reading time at 220 wpm, minimum one minute (§3.10).
    pub fn reading_minutes(&self) -> i64 {
        reading_minutes(self.word_count)
    }

    /// Composite social proof across all sources (§3.4).
    pub fn social_score(&self) -> f64 {
        composite_social_score(&self.social)
    }

    /// True when this story arrived via a feed of the given kind (§3.5).
    pub fn came_via(&self, kind: SourceKind) -> bool {
        self.sources.iter().any(|s| s.kind == kind)
    }

    /// Stable EPUB chapter id used by TOC and rating links (implementation notes §12).
    pub fn chapter_id(&self) -> String {
        format!("art-{}", self.best_entry_id)
    }
}

/// Estimated reading time at 220 wpm, minimum one minute.
pub fn reading_minutes(word_count: i64) -> i64 {
    (word_count.max(0) as f64 / 220.0).ceil().max(1.0) as i64
}

// ---------------------------------------------------------------------------
// Social proof (§3.4)
// ---------------------------------------------------------------------------

/// Social platforms we look up. `X` is reserved: no free API today (§3.4, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocialSource {
    Hn,
    Lobsters,
    Reddit,
    X,
}

impl SocialSource {
    /// Value stored in `social.source` (matches the CHECK constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            SocialSource::Hn => "hn",
            SocialSource::Lobsters => "lobsters",
            SocialSource::Reddit => "reddit",
            SocialSource::X => "x",
        }
    }

    /// Human-readable label used in chapter titles and stat lines (§3.7, §3.10).
    pub fn display_name(self) -> &'static str {
        match self {
            SocialSource::Hn => "HN",
            SocialSource::Lobsters => "Lobsters",
            SocialSource::Reddit => "Reddit",
            SocialSource::X => "X",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hn" => Some(SocialSource::Hn),
            "lobsters" => Some(SocialSource::Lobsters),
            "reddit" => Some(SocialSource::Reddit),
            "x" => Some(SocialSource::X),
            _ => None,
        }
    }
}

impl fmt::Display for SocialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A cached social-proof lookup for one article on one platform (`social` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialRef {
    pub article_id: ArticleId,
    pub source: SocialSource,
    /// Platform item id: HN `objectID`, lobsters story id, reddit fullname.
    pub item_id: Option<String>,
    pub score: i64,
    pub num_comments: i64,
    /// Link a human can open (HN item page, lobsters story, reddit permalink).
    pub item_url: Option<String>,
    pub fetched_at: Timestamp,
}

/// `log10(1+hn) + 0.7*log10(1+reddit) + log10(1+lobsters) + 0.5*log10(1+comments)` (§3.4).
///
/// Lives here rather than in `social/` because both the pre-filter and the EPUB
/// stat line need it.
pub fn composite_social_score(refs: &[SocialRef]) -> f64 {
    let mut score = 0.0;
    let mut comments = 0i64;
    for r in refs {
        let points = (r.score.max(0)) as f64;
        let weight = match r.source {
            SocialSource::Hn | SocialSource::Lobsters => 1.0,
            SocialSource::Reddit => 0.7,
            SocialSource::X => 0.0,
        };
        score += weight * (1.0 + points).log10();
        comments += r.num_comments.max(0);
    }
    score + 0.5 * (1.0 + comments as f64).log10()
}

// ---------------------------------------------------------------------------
// Curation (§3.5, §3.6)
// ---------------------------------------------------------------------------

/// DeepSeek stage-A output for one article (§3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmScore {
    /// 0–10.
    pub score: f64,
    pub category: String,
    /// ≤ 20 words.
    pub rationale: String,
    #[serde(default)]
    pub is_paywalled_guess: bool,
}

/// Deep assessment output. Step 5 replaces the legacy stage-A producer while
/// keeping its shape compatible for this transition step.
pub type Deep = LlmScore;

/// Personalized first-pass judgment cached in `article_assessments` (§10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Triage {
    /// Reader interest, clamped to 0–10.
    pub interest: f64,
    pub kind: String,
    /// At most twelve words in a conforming response.
    pub why: String,
    pub model: String,
    pub prompt_version: i64,
    pub assessed_at: Timestamp,
}

/// Cached LLM judgments accumulated for an article (§7.3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub triage: Option<Triage>,
    /// Filled in by step 5.
    pub deep: Option<Deep>,
}

/// An article carrying the state of the personalized curation pipeline (§18).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub article: Article,
    pub auto_include: bool,
    pub exploration: bool,
    pub signals: crate::curate::signals::Signals,
    pub assessment: Assessment,
    /// Filled in by step 5.
    pub utility: Option<f64>,
    /// Filled in by step 5.
    pub cluster: Option<i64>,
    pub admitted_by: Vec<String>,
    pub stage: String,
    pub excluded_reason: Option<String>,
}

impl Candidate {
    pub fn new(article: Article, auto_include: bool) -> Self {
        let signals = crate::curate::signals::Signals::baseline(&article);
        Self {
            article,
            auto_include,
            exploration: false,
            signals,
            assessment: Assessment::default(),
            utility: None,
            cluster: None,
            admitted_by: Vec::new(),
            stage: "eligible".into(),
            excluded_reason: None,
        }
    }

    /// Adapter retained until step 5 retires stage A and `ScoredArticle`.
    pub fn into_legacy_scored(self) -> ScoredArticle {
        ScoredArticle {
            prefilter_score: self.signals.preliminary.unwrap_or(0.0),
            social_score: self.signals.social.unwrap_or(0.0),
            llm: self.assessment.deep,
            triage: self.assessment.triage,
            auto_include: self.auto_include,
            exploration: self.exploration,
            admitted_by: self.admitted_by,
            article: self.article,
        }
    }
}

/// An article carrying every ranking signal computed so far (§3.5, §3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredArticle {
    pub article: Article,
    /// Heuristic pre-filter score, 0–100 (§3.5).
    pub prefilter_score: f64,
    /// Cached [`composite_social_score`] for the article.
    pub social_score: f64,
    /// `None` until stage A has run (or when `--skip-llm`).
    pub llm: Option<LlmScore>,
    /// Transitional metadata rendered by the editor until step 5 removes this type.
    #[serde(default)]
    pub triage: Option<Triage>,
    /// From `curation.always_include_feeds`: may be scored but never dropped (§3.5).
    pub auto_include: bool,
    #[serde(default)]
    pub exploration: bool,
    #[serde(default)]
    pub admitted_by: Vec<String>,
}

impl ScoredArticle {
    /// Ranking key for stage B: LLM score weighted with social proof (§3.6).
    pub fn combined_score(&self) -> f64 {
        let llm = self.llm.as_ref().map(|l| l.score).unwrap_or(0.0);
        llm * 10.0 + self.social_score * 4.0 + self.prefilter_score * 0.1
    }
}

/// One selected article with its section placement (§3.6 stage B).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub article: Article,
    /// One of `curation.sections` (or the reserved `World Briefing`).
    pub section: String,
    /// Order within the section, ascending.
    pub position: i64,
    pub is_lead: bool,
    /// Editor-written reason, at most 14 words (§13).
    pub why: Option<String>,
    /// Newspaper-abstract summary from stage C; `None` until editorial runs.
    pub summary: Option<String>,
    pub llm: Option<LlmScore>,
    /// Rendered comment chapter, when the article had social refs (§3.7).
    pub discussion: Option<Discussion>,
}

/// The day's final lineup: 15–25 picks grouped into sections (§3.6 stage B).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lineup {
    pub date: Date,
    /// Sorted by (section order, position).
    pub picks: Vec<Pick>,
    /// Section names in issue order; empty sections are omitted (§3.6).
    pub section_order: Vec<String>,
}

impl Lineup {
    /// Picks belonging to `section`, in position order.
    pub fn section_picks(&self, section: &str) -> Vec<&Pick> {
        let mut v: Vec<&Pick> = self.picks.iter().filter(|p| p.section == section).collect();
        v.sort_by_key(|p| p.position);
        v
    }

    pub fn lead(&self) -> Option<&Pick> {
        self.picks.iter().find(|p| p.is_lead)
    }

    pub fn total_words(&self) -> i64 {
        self.picks.iter().map(|p| p.article.word_count).sum()
    }
}

/// Stage-C editorial output (§3.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Editorial {
    /// "From the Editor", 250–400 words, already sanitized XHTML.
    pub front_page_html: String,
    /// Article id → 2–3 sentence newspaper abstract.
    pub summaries: BTreeMap<ArticleId, String>,
}

/// The taste profile that forms the DeepSeek system prompt (§3.6, `kv`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TasteProfile {
    /// Full ~600-word prompt document.
    pub text: String,
    pub version: i64,
    pub built_at: Timestamp,
}

// ---------------------------------------------------------------------------
// Comments (§3.7)
// ---------------------------------------------------------------------------

/// One comment node in a discussion tree (§3.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub points: Option<i64>,
    /// Sanitized comment body, ellipsized to 1,200 chars.
    pub text_html: String,
    /// 0 for top-level; rendering stops at depth 3.
    pub depth: usize,
    pub children: Vec<Comment>,
}

/// The comment tree fetched from one platform for one article (§3.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentThread {
    pub source: SocialSource,
    pub item_url: String,
    pub total_comments: i64,
    /// Top ~8 top-level threads by score.
    pub comments: Vec<Comment>,
}

/// A rendered discussion chapter: one per article, HN → Lobsters → Reddit (§3.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discussion {
    pub article_id: ArticleId,
    /// Chapter id, `disc-{entry_id}` (implementation notes §12).
    pub chapter_id: String,
    pub threads: Vec<CommentThread>,
}

impl Discussion {
    pub fn total_comments(&self) -> i64 {
        self.threads.iter().map(|t| t.total_comments).sum()
    }
}

// ---------------------------------------------------------------------------
// World briefing (§3.8)
// ---------------------------------------------------------------------------

/// Wikipedia Current Events portal digest for one day (§3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldBriefing {
    pub date: Date,
    /// Portal URL the content came from (also used for CC BY-SA attribution).
    pub source_url: String,
    /// Optional synthesized overview of the completed day's events.
    pub overview: Option<String>,
    /// Categories in portal order, including every nested list item.
    pub sections: Vec<WorldBriefingSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldBriefingSection {
    pub title: String,
    pub events: Vec<WorldEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldEvent {
    /// Stable positional key such as `s1-e2-1`.
    pub id: String,
    /// Plain source text from the Wikipedia list item (excluding child lists).
    pub source_text: String,
    /// Same-host English Wikipedia article links found in this list item.
    pub links: Vec<String>,
    pub children: Vec<WorldEvent>,
    /// LLM enrichment, attached only to leaf news statements.
    pub summary: Option<String>,
}

/// Reserved section name for [`WorldBriefing`] — never offered to the LLM (§3.6).
pub const WORLD_BRIEFING_SECTION: &str = "World Briefing";

// ---------------------------------------------------------------------------
// Issue assembly (§3.10)
// ---------------------------------------------------------------------------

/// Which of the two editions is being built (§3.10).
///
/// The declaration order is the listing order: standard first, then X4. The
/// OPDS feed sorts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edition {
    /// 1200px images, full CSS.
    Standard,
    /// Grayscale, 480×800, simplified CSS — input for the XTC converter.
    X4,
}

impl Edition {
    /// Every edition, in the order they are listed and built (§3.10).
    pub const ALL: [Edition; 2] = [Edition::Standard, Edition::X4];

    /// Filename suffix: `""` / `" (X4)"` (§3.11).
    pub fn file_suffix(self) -> &'static str {
        match self {
            Edition::Standard => "",
            Edition::X4 => " (X4)",
        }
    }

    /// Recover the edition from a published filename's stem (§3.11).
    ///
    /// The OPDS feed is rebuilt by scanning the publish directory, so the
    /// filename is the only record of which edition a file is.
    pub fn from_file_stem(stem: &str) -> Edition {
        if stem.ends_with(Edition::X4.file_suffix()) {
            Edition::X4
        } else {
            Edition::Standard
        }
    }
}

/// Issue-level metadata rendered on the cover, front page and OPF (§3.10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueMeta {
    pub date: Date,
    /// Days since the first issue; EPUB3 `group-position`.
    pub issue_number: i64,
    pub generated_at: Timestamp,
    /// "Friday, August 15, 2026".
    pub display_date: String,
    pub article_count: i64,
    pub section_count: i64,
    pub total_words: i64,
    pub reading_minutes: i64,
}

impl IssueMeta {
    /// The issue's name, without an edition tag: "The Daily EPUB — 2026-08-15".
    pub fn title(&self) -> String {
        format!("The Daily EPUB — {}", self.date)
    }

    /// `dc:title` for one edition: [`title`](Self::title) plus the edition tag
    /// (§3.10).
    ///
    /// Both editions land in the same BookOrbit library, and BookOrbit — like
    /// every OPDS client — lists books by `dc:title`. Carrying the distinction
    /// only in the filename makes them indistinguishable everywhere except the
    /// per-book file listing, so the title and the filename share one suffix.
    pub fn title_for(&self, edition: Edition) -> String {
        format!("{}{}", self.title(), edition.file_suffix())
    }

    /// "22 articles · ~1h 45m read · 6 sections" (§3.10).
    pub fn stats_line(&self) -> String {
        let (h, m) = (self.reading_minutes / 60, self.reading_minutes % 60);
        let time = if h > 0 {
            format!("~{h}h {m}m read")
        } else {
            format!("~{m}m read")
        };
        format!(
            "{} articles · {} · {} sections",
            self.article_count, time, self.section_count
        )
    }
}

/// Everything the EPUB builder needs; fully materialized before rendering (§3.10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub meta: IssueMeta,
    pub lineup: Lineup,
    pub editorial: Editorial,
    pub world_briefing: Option<WorldBriefing>,
    /// Colophon facts: models used, token cost, feed counts (§3.10).
    pub colophon: Colophon,
}

/// Resolved model names printed in the colophon (§15.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Models {
    pub bulk: String,
    pub editor: String,
    pub summaries: String,
}

/// Back-matter facts printed in the colophon chapter (§3.10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Colophon {
    pub provider_costs: BTreeMap<String, f64>,
    pub models: Models,
    pub entries_fetched: i64,
    pub feeds_seen: i64,
    pub candidates: i64,
    pub cost_usd: f64,
    pub generator_version: String,
}

/// A downloaded, re-encoded image embedded in an edition (§3.10 images).
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAsset {
    /// Manifest id, unique within the issue.
    pub id: String,
    /// Path inside the EPUB, e.g. `images/art-1234-0.jpg`.
    pub href: String,
    pub mime: String,
    pub data: Vec<u8>,
    pub alt: String,
    pub caption: Option<String>,
    /// The original remote URL, used to rewrite `<img src>`.
    pub source_url: String,
}

/// A file produced by the build/publish stages (§3.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub edition: Edition,
    pub path: PathBuf,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Feedback (§3.9)
// ---------------------------------------------------------------------------

/// Explicit reader verdict embedded in rating-link URLs (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    #[serde(rename = "loved", alias = "up")]
    Loved,
    #[serde(rename = "good")]
    Good,
    #[serde(rename = "down")]
    NotForMe,
}

impl Vote {
    /// Stable path segment used in rating links (§6.1).
    pub fn as_str(self) -> &'static str {
        match self {
            Vote::Loved => "loved",
            Vote::Good => "good",
            Vote::NotForMe => "down",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "loved" | "up" => Some(Vote::Loved),
            "good" => Some(Vote::Good),
            "down" => Some(Vote::NotForMe),
            _ => None,
        }
    }

    pub fn value(self, cfg: &crate::config::FeedbackConfig) -> f64 {
        match self {
            Vote::Loved => cfg.loved_value,
            Vote::Good => cfg.good_value,
            Vote::NotForMe => cfg.not_for_me_value,
        }
    }
}

/// One append-only feedback event (`rating_events`, §6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingEvent {
    pub id: i64,
    pub article_id: ArticleId,
    pub issue_date: Option<Date>,
    pub kind: String,
    pub source: String,
    pub label: String,
    pub value: f64,
    pub note: Option<String>,
    pub event_at: Timestamp,
}

/// Descriptive deep-assessment facets (§12.1), populated beginning in step 5.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facets {
    pub format: Option<String>,
    pub depth: Option<String>,
    pub evidence: Option<String>,
    pub commerciality: Option<String>,
    pub topic_group: Option<String>,
    pub technicality: Option<String>,
    pub locality: Option<String>,
    pub specific_topics: Option<Vec<String>>,
}

/// The current explicit verdict for an article, enriched for prompts (§6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatedArticle {
    pub article_id: ArticleId,
    pub issue_date: Option<Date>,
    pub title: String,
    pub feed_title: String,
    pub summary: Option<String>,
    pub facets: Option<Facets>,
    pub note: Option<String>,
    pub value: f64,
    pub label: String,
    pub event_at: Timestamp,
}

// ---------------------------------------------------------------------------
// LLM accounting (§3.6 cost guardrail)
// ---------------------------------------------------------------------------

/// Token counters accumulated across every DeepSeek call in a run (§3.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Cache-miss input tokens (billed at the full input rate).
    pub input_tokens: i64,
    /// Prefix-cache reads (billed at the provider's cache-read rate).
    pub cached_tokens: i64,
    /// Tokens written into a prompt cache (Anthropic only).
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
}

impl TokenUsage {
    pub fn add(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.output_tokens += other.output_tokens;
    }

    /// USD cost given the per-1M-token prices from `[deepseek]` config (§3.6).
    pub fn cost_usd(
        &self,
        price_input: f64,
        price_cache_write: f64,
        price_cache_read: f64,
        price_output: f64,
    ) -> f64 {
        (self.input_tokens as f64 * price_input
            + self.cache_write_tokens as f64 * price_cache_write
            + self.cached_tokens as f64 * price_cache_read
            + self.output_tokens as f64 * price_output)
            / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> Timestamp {
        "2026-08-15T05:30:00Z".parse().unwrap()
    }

    fn social(source: SocialSource, score: i64, comments: i64) -> SocialRef {
        SocialRef {
            article_id: 1,
            source,
            item_id: Some("1".into()),
            score,
            num_comments: comments,
            item_url: None,
            fetched_at: ts(),
        }
    }

    #[test]
    fn composite_social_score_matches_spec_formula() {
        let refs = vec![
            social(SocialSource::Hn, 342, 210),
            social(SocialSource::Reddit, 99, 40),
        ];
        let expected = (343f64).log10() + 0.7 * (100f64).log10() + 0.5 * (251f64).log10();
        assert!((composite_social_score(&refs) - expected).abs() < 1e-9);
        assert_eq!(composite_social_score(&[]), 0.0);
    }

    #[test]
    fn stats_line_and_reading_time() {
        assert_eq!(reading_minutes(0), 1);
        assert_eq!(reading_minutes(440), 2);
        let meta = IssueMeta {
            date: "2026-08-15".parse().unwrap(),
            issue_number: 1,
            generated_at: ts(),
            display_date: "Friday, August 15, 2026".into(),
            article_count: 22,
            section_count: 6,
            total_words: 23_000,
            reading_minutes: 105,
        };
        assert_eq!(meta.stats_line(), "22 articles · ~1h 45m read · 6 sections");
        assert_eq!(meta.title(), "The Daily EPUB — 2026-08-15");
        assert_eq!(
            meta.title_for(Edition::Standard),
            "The Daily EPUB — 2026-08-15"
        );
        assert_eq!(
            meta.title_for(Edition::X4),
            "The Daily EPUB — 2026-08-15 (X4)"
        );
        // Title and filename carry the same tag, so a book found in the library
        // maps back to a file without guessing.
        assert!(
            meta.title_for(Edition::X4)
                .ends_with(Edition::X4.file_suffix())
        );
    }

    #[test]
    fn vote_and_social_source_round_trip() {
        let feedback = crate::config::FeedbackConfig::default();
        assert_eq!(Vote::parse("loved"), Some(Vote::Loved));
        assert_eq!(Vote::parse("up"), Some(Vote::Loved));
        assert_eq!(Vote::parse("good"), Some(Vote::Good));
        assert_eq!(Vote::parse("down"), Some(Vote::NotForMe));
        assert_eq!(Vote::Loved.as_str(), "loved");
        assert_eq!(Vote::Good.as_str(), "good");
        assert_eq!(Vote::NotForMe.as_str(), "down");
        assert_eq!(Vote::Loved.value(&feedback), 1.0);
        assert_eq!(Vote::Good.value(&feedback), 0.35);
        assert_eq!(Vote::NotForMe.value(&feedback), -1.0);
        assert_eq!(serde_json::to_string(&Vote::NotForMe).unwrap(), "\"down\"");
        assert_eq!(
            SocialSource::parse("lobsters"),
            Some(SocialSource::Lobsters)
        );
        assert_eq!(SocialSource::Hn.as_str(), "hn");
    }
}
