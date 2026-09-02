//! Curation pipeline: pre-filter → LLM scoring → selection → editorial (spec §3.5, §3.6).
//!
//! ```text
//! ~400 articles ─prefilter─▶ ~120 candidates ─stage A─▶ scored ─stage B─▶ lineup ─stage C─▶ editorial
//! ```
//!
//! [`Curator`] is the thin orchestration layer the `generate` pipeline calls; the
//! interesting logic lives in the stage modules. Every stage is safe to run with
//! `llm == None` (`--skip-llm`): the prefilter order stands in for selection and
//! feed excerpts stand in for summaries (notes §6).

pub mod editorial;
pub mod embedding;
pub mod llm;
pub mod prefilter;
pub mod profile;
pub mod score;
pub mod select;
pub mod signals;
pub mod telemetry;

use jiff::civil::Date;

use crate::config::Config;
use crate::db::Db;
use crate::types::{Article, Editorial, Lineup, ScoredArticle};

/// Runs the three curation stages against one day's articles (§3.5, §3.6).
pub struct Curator {
    pub config: Config,
    pub db: Db,
    pub llm: Option<llm::LlmClient>,
}

impl Curator {
    /// `llm == None` corresponds to `--skip-llm`: prefilter order is used for
    /// selection and feed excerpts stand in for summaries (notes §6).
    pub fn new(config: Config, db: Db, llm: Option<llm::LlmClient>) -> Self {
        Self { config, db, llm }
    }

    /// Heuristic pre-filter: 300–500 articles → `prefilter_keep` (§3.5).
    ///
    /// Also persists each candidate's `prefilter_score` for the day so that a
    /// re-run of the same date is idempotent (notes §12).
    pub async fn prefilter(
        &self,
        articles: Vec<Article>,
        date: Date,
    ) -> anyhow::Result<Vec<ScoredArticle>> {
        let span = tracing::info_span!("prefilter", articles = articles.len());
        let _guard = span.enter();

        let ctx = prefilter::PrefilterContext::load(&self.db, date).await?;
        let candidates = prefilter::run(articles, &ctx, &self.config);
        for candidate in &candidates {
            if candidate.article.id == 0 {
                continue; // not persisted yet (dry run over synthetic articles)
            }
            if let Err(e) = self
                .db
                .upsert_score(
                    candidate.article.id,
                    date,
                    Some(candidate.prefilter_score),
                    None,
                )
                .await
            {
                tracing::warn!(article_id = candidate.article.id, error = %e,
                    "could not persist the prefilter score");
            }
        }
        Ok(candidates)
    }

    /// Stage A: batched LLM scoring of the surviving candidates (§3.6).
    ///
    /// A no-op under `--skip-llm`. Scores are persisted per `(article, date)`.
    pub async fn score(&self, candidates: &mut [ScoredArticle], date: Date) -> anyhow::Result<()> {
        let Some(llm) = self.llm.as_ref() else {
            tracing::info!("--skip-llm: stage A scoring skipped");
            return Ok(());
        };
        let span = tracing::info_span!("llm_score", candidates = candidates.len());
        let _guard = span.enter();

        let scored = score::score_all(
            llm,
            candidates,
            self.config.deepseek.score_batch_size,
            &self.config.curation.sections,
            self.config.deepseek.score_temperature,
        )
        .await?;
        tracing::info!(scored, total = candidates.len(), "stage A complete");

        for candidate in candidates.iter() {
            if candidate.article.id == 0 {
                continue;
            }
            if let Some(llm_score) = candidate.llm.as_ref()
                && let Err(e) = self
                    .db
                    .upsert_score(candidate.article.id, date, None, Some(llm_score))
                    .await
            {
                tracing::warn!(article_id = candidate.article.id, error = %e,
                    "could not persist the llm score");
            }
        }
        Ok(())
    }

    /// Stage B: single-call lineup selection into sections (§3.6).
    pub async fn select(
        &self,
        candidates: Vec<ScoredArticle>,
        date: Date,
    ) -> anyhow::Result<Lineup> {
        let sections = &self.config.curation.sections;
        let target = self.config.target_article_count;
        let Some(llm) = self.llm.as_ref() else {
            tracing::info!("--skip-llm: selecting by prefilter order");
            return Ok(select::select_without_llm(
                candidates, sections, target, date,
            ));
        };
        let span = tracing::info_span!("llm_select", candidates = candidates.len());
        let _guard = span.enter();

        match select::select(llm, candidates.clone(), sections, target, date).await {
            Ok(lineup) => Ok(lineup),
            Err(e) => {
                tracing::error!(error = %e,
                    "stage B selection failed; falling back to prefilter order");
                Ok(select::select_without_llm(
                    candidates, sections, target, date,
                ))
            }
        }
    }

    /// Stage C: per-article summaries, section intros and the front page (§3.6).
    ///
    /// Never fails the run: a budget trip or an API error degrades to excerpts.
    pub async fn editorial(&self, lineup: &Lineup) -> anyhow::Result<Editorial> {
        let Some(llm) = self.llm.as_ref() else {
            tracing::info!("--skip-llm: using feed excerpts as summaries");
            return Ok(editorial::fallback_editorial(lineup));
        };
        let span = tracing::info_span!("llm_editorial", picks = lineup.picks.len());
        let _guard = span.enter();
        Ok(editorial::run(llm, lineup, self.config.deepseek.editorial_temperature).await)
    }
}

// ---------------------------------------------------------------------------
// Small text helpers shared by the prompt builders
// ---------------------------------------------------------------------------

/// Crude token estimate: DeepSeek averages ~4 characters per token for English
/// prose. Only used to size prompt budgets (§3.6 stage C).
pub fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Strip markup and collapse whitespace, so article bodies can go into prompts
/// as plain text (cheaper and less confusing for the model than raw HTML).
///
/// Deliberately not [`crate::html::html_to_text`]: this one collapses runs of
/// whitespace and never builds a DOM, because it runs over every candidate
/// body on every run and only has to be good enough to size a prompt.
pub fn prompt_text(html: &str) -> String {
    /// Does `tail` open the named element, i.e. `<name` or `</name`?
    fn opens(tail: &str, name: &str) -> bool {
        let bytes = tail.as_bytes();
        bytes.len() > name.len() && bytes[1..=name.len()].eq_ignore_ascii_case(name.as_bytes())
    }

    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(ch) = rest.chars().next() {
        if ch != '<' {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        // Drop <script>/<style> bodies wholesale rather than reading them aloud.
        for (name, close) in [("script", "</script"), ("style", "</style")] {
            if opens(rest, name) {
                rest = match rest[1..].find(close) {
                    Some(idx) => &rest[1 + idx + close.len()..],
                    None => "",
                };
                break;
            }
        }
        // A tag becomes a word boundary.
        rest = match rest.find('>') {
            Some(idx) => &rest[idx + 1..],
            None => "",
        };
        out.push(' ');
    }

    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&rsquo;", "'")
        .replace("&mdash;", "—");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First `max_words` words of `text`, with an ellipsis when truncated.
pub fn truncate_words(text: &str, max_words: usize) -> String {
    let mut words = text.split_whitespace();
    let head: Vec<&str> = words.by_ref().take(max_words).collect();
    let mut out = head.join(" ");
    if words.next().is_some() {
        out.push('…');
    }
    out
}

/// Truncate to roughly `max_tokens` tokens on a word boundary (§3.6 stage C).
pub fn truncate_tokens(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut cut = max_chars.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let slice = &text[..cut];
    let slice = slice
        .rsplit_once(' ')
        .map(|(head, _)| head)
        .unwrap_or(slice);
    format!("{slice}…")
}

/// Minimal XHTML escaping for text we drop into generated markup (§3.10).
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Render plain text (possibly with blank-line paragraphs) as XHTML paragraphs.
pub fn text_to_paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", escape_html(&p.replace('\n', " "))))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let html = "<h1>Title</h1><p>First &amp; best.</p><script>alert('x')</script>\
                    <p>Second<br/>line</p><style>p{color:red}</style>";
        assert_eq!(prompt_text(html), "Title First & best. Second line");
        assert_eq!(prompt_text(""), "");
        assert_eq!(prompt_text("no markup at all"), "no markup at all");
        // Unicode survives byte-wise walking.
        assert_eq!(prompt_text("<p>café — naïve</p>"), "café — naïve");
    }

    #[test]
    fn word_and_token_truncation() {
        assert_eq!(truncate_words("one two three", 5), "one two three");
        assert_eq!(truncate_words("one two three", 2), "one two…");
        let long = "word ".repeat(1000);
        // 10 tokens ≈ 40 characters, cut back to a word boundary, plus the ellipsis.
        let cut = truncate_tokens(&long, 10);
        assert!(cut.len() <= 43, "{}", cut.len());
        assert!(cut.split_whitespace().count() <= 10);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate_tokens("short", 10), "short");
        assert!(approx_tokens("abcd") <= 1);
    }

    #[test]
    fn escaping_and_paragraphs() {
        assert_eq!(escape_html("a<b>&'\""), "a&lt;b&gt;&amp;&#39;&quot;");
        assert_eq!(
            text_to_paragraphs("One\nline.\n\nTwo <b>."),
            "<p>One line.</p>\n<p>Two &lt;b&gt;.</p>"
        );
        assert_eq!(text_to_paragraphs("   "), "");
    }
}
