//! Run report — counts, token usage, cost, timings, status (spec §3.13 `runs`, §3.12
//! `/issues.json`).
//!
//! `generate` builds one of these, prints it at the end of the run and stores the
//! serialized form in `runs` / `issues.report_json`.

use std::collections::BTreeMap;
use std::fmt;

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use crate::types::TokenUsage;

/// Terminal state of a run, stored in `runs.status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    /// Everything completed.
    Ok,
    /// The issue was produced but a best-effort stage failed (social, XTC,
    /// world briefing, images) or the cost guardrail tripped (§3.6).
    Degraded,
    /// No issue was produced.
    Failed,
    /// `--dry-run`: nothing was published.
    DryRun,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Degraded => "degraded",
            RunStatus::Failed => "failed",
            RunStatus::DryRun => "dry_run",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-stage article counts as the pipeline narrows the day's feed volume (§2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageCounts {
    /// Entries returned by Miniflux inside the lookback window (§3.1).
    pub entries_fetched: i64,
    /// Distinct feeds those entries came from.
    pub feeds_seen: i64,
    /// Entries dropped as non-articles (video/audio/empty title) (§3.2).
    pub entries_dropped: i64,
    /// Deduped article clusters (§3.2).
    pub articles: i64,
    /// Clusters that merged ≥ 2 entries.
    pub duplicates_merged: i64,
    /// Articles whose full text was fetched + extracted (§3.3).
    pub extracted: i64,
    /// Articles left with only an excerpt (§3.3).
    pub excerpt_only: i64,
    /// Social lookups that returned a hit (§3.4).
    pub social_hits: i64,
    /// Articles passing hygiene and eligible for personalized signals.
    pub eligible: i64,
    /// Eligible articles with a valid embedding.
    pub embedded: i64,
    /// Current rated articles with a valid embedding.
    pub rated_with_embeddings: i64,
    /// Articles surviving the heuristic pre-filter (§3.5).
    pub candidates: i64,
    /// Articles scored by the LLM (§3.6 stage A).
    pub llm_scored: i64,
    /// Articles in the final lineup (§3.6 stage B).
    pub selected: i64,
    /// Discussion chapters rendered (§3.7).
    pub discussions: i64,
    /// Images embedded across both editions (§3.10).
    pub images_embedded: i64,
}

/// Wall-clock milliseconds per pipeline stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTimings(pub BTreeMap<String, i64>);

impl StageTimings {
    pub fn record(&mut self, stage: &str, millis: i64) {
        *self.0.entry(stage.to_string()).or_insert(0) += millis;
    }

    pub fn total_ms(&self) -> i64 {
        self.0.values().sum()
    }
}

/// The full summary of one `generate` invocation (§3.13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub date: Date,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub status: RunStatus,
    pub counts: StageCounts,
    pub usage: TokenUsage,
    /// Voyage document/query tokens and cost for this run.
    pub voyage_tokens: i64,
    pub voyage_cost_usd: f64,
    pub cost_usd: f64,
    pub timings: StageTimings,
    /// Ingest window actually used, RFC3339 (§3.1).
    pub window_start: Option<Timestamp>,
    pub window_end: Option<Timestamp>,
    /// Entry counts per feed title, for spotting noisy feeds (M1 verification).
    pub per_feed_counts: BTreeMap<String, i64>,
    /// Non-fatal problems from best-effort stages (notes §3).
    pub warnings: Vec<String>,
    /// Fatal error message when `status == Failed`.
    pub error: Option<String>,
}

impl RunReport {
    pub fn new(date: Date, started_at: Timestamp) -> Self {
        Self {
            date,
            started_at,
            finished_at: None,
            status: RunStatus::Running,
            counts: StageCounts::default(),
            usage: TokenUsage::default(),
            voyage_tokens: 0,
            voyage_cost_usd: 0.0,
            cost_usd: 0.0,
            timings: StageTimings::default(),
            window_start: None,
            window_end: None,
            per_feed_counts: BTreeMap::new(),
            warnings: Vec::new(),
            error: None,
        }
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::warn!(target: "daily_epub::report", "{msg}");
        self.warnings.push(msg);
    }

    pub fn fail(&mut self, finished_at: Timestamp, err: impl fmt::Display) {
        self.finished_at = Some(finished_at);
        self.status = RunStatus::Failed;
        self.error = Some(err.to_string());
    }

    /// Stamp the end time, compute cost from [`TokenUsage`] and settle the status.
    pub fn finish(
        &mut self,
        finished_at: Timestamp,
        price_input: f64,
        price_cached: f64,
        price_output: f64,
    ) {
        self.finished_at = Some(finished_at);
        self.cost_usd =
            self.usage.cost_usd(price_input, price_cached, price_output) + self.voyage_cost_usd;
        if self.status == RunStatus::Running {
            self.status = if self.warnings.is_empty() {
                RunStatus::Ok
            } else {
                RunStatus::Degraded
            };
        }
    }

    /// Total wall-clock duration in seconds, when finished.
    pub fn duration_secs(&self) -> Option<i64> {
        self.finished_at
            .map(|end| (end.as_second() - self.started_at.as_second()).max(0))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Compact human-readable summary printed at the end of `generate`.
    pub fn summary_line(&self) -> String {
        format!(
            "{} [{}] {} entries → {} articles → {} eligible → {} candidates → {} selected · ${:.4} · {}s",
            self.date,
            self.status,
            self.counts.entries_fetched,
            self.counts.articles,
            self.counts.eligible,
            self.counts.candidates,
            self.counts.selected,
            self.cost_usd,
            self.duration_secs().unwrap_or(0),
        )
    }

    /// Feeds ordered by entry count, descending — the M1 dry-run breakdown.
    pub fn top_feeds(&self, limit: usize) -> Vec<(&str, i64)> {
        let mut v: Vec<(&str, i64)> = self
            .per_feed_counts
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v.truncate(limit);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn finish_computes_cost_and_status() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.usage.add(TokenUsage {
            input_tokens: 1_000_000,
            cached_tokens: 1_000_000,
            output_tokens: 1_000_000,
        });
        r.finish(ts("2026-08-15T05:36:00Z"), 0.14, 0.0028, 0.28);
        assert_eq!(r.status, RunStatus::Ok);
        assert!((r.cost_usd - 0.4228).abs() < 1e-9);
        assert_eq!(r.duration_secs(), Some(360));
    }

    #[test]
    fn warnings_degrade_the_run() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.warn("xtc converter missing");
        r.finish(ts("2026-08-15T05:31:00Z"), 0.14, 0.0028, 0.28);
        assert_eq!(r.status, RunStatus::Degraded);
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn serializes_round_trip() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.counts.entries_fetched = 412;
        r.per_feed_counts.insert("Hacker News".into(), 30);
        r.per_feed_counts.insert("Lobsters".into(), 12);
        r.timings.record("ingest", 1500);
        r.finish(ts("2026-08-15T05:31:00Z"), 0.14, 0.0028, 0.28);
        let json = r.to_json();
        let back: RunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.top_feeds(1), vec![("Hacker News", 30)]);
        assert_eq!(back.timings.total_ms(), 1500);
        assert!(back.summary_line().contains("412 entries"));
    }
}
