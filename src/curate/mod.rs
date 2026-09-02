//! Personalized curation: signals → triage → admission → assessment → editor.
//!
//! ```text
//! ~400 eligible ─triage─▶ admission (120) ─assess/rank─▶ shortlist (60) ─editor─▶ editorial
//! ```
//!
//! [`Curator`] is the thin orchestration layer the `generate` pipeline calls; the
//! interesting logic lives in the stage modules. Every stage is safe to run with
//! no provider at all (`--skip-llm`): the cheap-signal blend stands in for selection
//! and feed excerpts stand in for summaries (notes §6). Scoring runs on the bulk
//! client; selection and editorial on the editor with per-call bulk fallback.

pub mod admit;
pub mod assess;
pub mod editor;
pub mod editorial;
pub mod embedding;
pub mod llm;
pub mod prefilter;
pub mod profile;
pub mod rank;
pub mod signals;
pub mod telemetry;
pub mod triage;

use jiff::{Timestamp, civil::Date};

use crate::config::Config;
use crate::db::Db;
use crate::types::{Candidate, Editorial, Lineup};

/// Runs the LLM curation stages against one day's articles (§12, §13, §14).
pub struct Curator {
    pub config: Config,
    pub db: Db,
    pub llms: llm::Llms,
}

impl Curator {
    /// An empty [`llm::Llms`] corresponds to `--skip-llm`: cheap-signal order is
    /// used for selection and feed excerpts stand in for summaries (notes §6).
    /// With only `bulk`, every editor call runs on DeepSeek (§4.2).
    pub fn new(config: Config, db: Db, llms: llm::Llms) -> Self {
        Self { config, db, llms }
    }

    /// Deep assessment of the admitted set on the bulk client (§12.1), reusing
    /// cached `article_assessments` rows within `assessment_reuse_days`.
    ///
    /// A no-op under `--skip-llm`: like triage, nothing is read or written and
    /// utility falls back to the present signals (§12.3, §17). When the bulk
    /// provider is down or its budget trips, cached rows are still reused and
    /// the failed batches simply stay unassessed.
    pub async fn assess(
        &self,
        candidates: &mut [Candidate],
        rescore: bool,
        profile_version: Option<i64>,
        assessed_at: Timestamp,
    ) -> anyhow::Result<usize> {
        let Some(bulk) = self.llms.bulk.as_ref() else {
            tracing::info!("--skip-llm: deep assessment skipped");
            return Ok(0);
        };
        let span = tracing::info_span!("llm_assess", candidates = candidates.len());
        let _guard = span.enter();
        assess::run(
            &self.db,
            Some(bulk),
            &self.config.deepseek.model,
            candidates,
            self.config.deepseek.deep_batch_size,
            self.config.deepseek.max_concurrent_requests,
            self.config.curation.ranking.assessment_reuse_days,
            rescore,
            profile_version,
            assessed_at,
            self.config.deepseek.score_temperature,
            &self.config.curation.sections,
        )
        .await
    }

    /// The editor: one call that assembles the issue from the shortlist (§13).
    pub async fn select(&self, candidates: Vec<Candidate>, date: Date) -> anyhow::Result<Lineup> {
        let sections = &self.config.curation.sections;
        let soft_target = self.config.target_article_count;
        let hard_max = self.config.curation.max_article_count;
        let span = tracing::info_span!("llm_editor", candidates = candidates.len());
        let _guard = span.enter();
        match editor::select(
            &self.llms,
            candidates.clone(),
            sections,
            soft_target,
            hard_max,
            date,
        )
        .await
        {
            Ok(lineup) => Ok(lineup),
            Err(error) => {
                tracing::error!(%error, "editor and bulk fallback failed; selecting heuristically");
                Ok(editor::select_without_llm(
                    candidates,
                    sections,
                    soft_target,
                    hard_max,
                    date,
                ))
            }
        }
    }

    /// Stage C: per-article summaries, section intros and the front page (§3.6).
    ///
    /// Never fails the run: a budget trip or an API error degrades to excerpts.
    pub async fn editorial(&self, lineup: &Lineup) -> anyhow::Result<Editorial> {
        if self.llms.editor_or_bulk().is_none() {
            tracing::info!("--skip-llm: using feed excerpts as summaries");
            return Ok(editorial::fallback_editorial(lineup));
        }
        let span = tracing::info_span!("llm_editorial", picks = lineup.picks.len());
        let _guard = span.enter();
        Ok(editorial::run(
            &self.llms,
            lineup,
            &self.config.editorial,
            self.config.deepseek.editorial_temperature,
        )
        .await)
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
