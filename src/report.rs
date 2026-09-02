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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// The neighbour signal's gate ramp, 0–1 (§9.2); 0 means the signal is absent.
    #[serde(default)]
    pub knn_gate: f64,
    /// The feed-affinity gate ramp, 0–1 (§9.3).
    #[serde(default)]
    pub feed_gate: f64,
    /// Explicit verdicts rendered into the system prompt (§8.4).
    #[serde(default)]
    pub verdicts_in_prompt: i64,
    /// Articles with a reusable or newly produced triage assessment.
    pub triaged: i64,
    /// Articles admitted to close reading.
    pub admitted: i64,
    /// First admitting retriever counts.
    pub admitted_by: BTreeMap<String, i64>,
    pub exploration_admitted: i64,
    pub exploration_selected: i64,
    /// Deep assessments, whether reused or newly produced.
    pub assessed: i64,
    /// Candidates shown to the editor after diversification.
    pub shortlisted: i64,
    /// Leader clusters formed over the deep set.
    pub clusters: i64,
    /// Admitted deep-set count retained for the colophon and runs table.
    pub candidates: i64,
    /// Articles in the final lineup (§3.6 stage B).
    pub selected: i64,
    /// Discussion chapters rendered (§3.7).
    pub discussions: i64,
    /// Images embedded across both editions (§3.10).
    pub images_embedded: i64,
}

/// Key under which Voyage usage sits in `provider_costs` (§7.6). Its token count
/// is embedding input, so it stays out of the LLM `usage` aggregate.
pub const VOYAGE_PROVIDER: &str = "voyage";

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

/// Usage and computed cost for one provider in this run (§5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    #[serde(flatten)]
    pub usage: TokenUsage,
    pub cost_usd: f64,
}

/// The full summary of one `generate` invocation (§3.13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub date: Date,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub status: RunStatus,
    pub counts: StageCounts,
    /// Aggregate usage retained for the legacy `runs` columns.
    pub usage: TokenUsage,
    /// LLM provider-keyed usage and cost written to `runs.provider_costs_json`.
    pub provider_costs: BTreeMap<String, ProviderUsage>,
    /// Resolved curation/editorial/model settings for this run.
    pub config_json: serde_json::Value,
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
            provider_costs: BTreeMap::new(),
            config_json: serde_json::Value::Null,
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

    /// Stamp the end time, total provider costs (LLM providers plus Voyage) and
    /// settle the status.
    ///
    /// Voyage is counted once: through its `provider_costs` entry when the
    /// pipeline recorded one, else through `voyage_cost_usd`.
    pub fn finish(&mut self, finished_at: Timestamp) {
        self.finished_at = Some(finished_at);
        self.usage = TokenUsage::default();
        self.cost_usd = 0.0;
        for (provider, usage) in &self.provider_costs {
            self.cost_usd += usage.cost_usd;
            if provider != VOYAGE_PROVIDER {
                self.usage.add(usage.usage);
            }
        }
        if !self.provider_costs.contains_key(VOYAGE_PROVIDER) {
            self.cost_usd += self.voyage_cost_usd;
        }
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

    /// "23m12s" / "48s" for the log block and `stats`.
    pub fn format_duration(secs: i64) -> String {
        let secs = secs.max(0);
        if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        }
    }

    /// The four-line info block of §15.4 (`curation:`, `admission:`,
    /// `preference:`, `providers:`), logged once per run and printed by the CLI.
    pub fn info_block(&self) -> [String; 4] {
        let c = &self.counts;
        let admitted = |name: &str| c.admitted_by.get(name).copied().unwrap_or(0);
        let gate = |value: f64| {
            if value > 0.0 {
                format!("{value:.2}")
            } else {
                "off".to_string()
            }
        };
        let providers = self
            .provider_costs
            .iter()
            .map(|(provider, usage)| format!("{provider} ${:.2}", usage.cost_usd))
            .collect::<Vec<_>>()
            .join(" · ");
        let providers = if providers.is_empty() {
            "none".to_string()
        } else {
            providers
        };
        [
            format!(
                "curation: {} considered → {} eligible → {} triaged → {} assessed → {} shortlisted → {} selected",
                c.articles, c.eligible, c.triaged, c.assessed, c.shortlisted, c.selected
            ),
            format!(
                "admission: triage {} · interest {} · knn {} · exploration {} · blend {} · auto {}",
                admitted("triage"),
                admitted("interest"),
                admitted("knn"),
                admitted("exploration"),
                admitted("blend"),
                admitted("auto_include"),
            ),
            format!(
                "preference: {} rated w/ embeddings → knn {} · feed {} · {} verdicts in prompt",
                c.rated_with_embeddings,
                gate(c.knn_gate),
                gate(c.feed_gate),
                c.verdicts_in_prompt
            ),
            format!(
                "providers: {providers} · total ${:.2} · {}",
                self.cost_usd,
                Self::format_duration(self.duration_secs().unwrap_or(0))
            ),
        ]
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

    fn usage(input: i64, cached: i64, cache_write: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_tokens: cached,
            cache_write_tokens: cache_write,
            output_tokens: output,
        }
    }

    #[test]
    fn finish_totals_provider_costs_and_settles_status() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.provider_costs.insert(
            "deepseek".into(),
            ProviderUsage {
                usage: usage(1_000_000, 1_000_000, 0, 1_000_000),
                cost_usd: 0.4228,
            },
        );
        r.provider_costs.insert(
            "anthropic".into(),
            ProviderUsage {
                usage: usage(100, 3_000, 2_000, 800),
                cost_usd: 0.05,
            },
        );
        r.voyage_tokens = 250_000;
        r.voyage_cost_usd = 0.005;
        r.finish(ts("2026-08-15T05:36:00Z"));
        assert_eq!(r.status, RunStatus::Ok);
        // LLM providers plus Voyage; Voyage tokens stay out of the LLM aggregate.
        assert!((r.cost_usd - 0.4778).abs() < 1e-9);
        // The legacy aggregate columns are the sum across providers.
        assert_eq!(r.usage, usage(1_000_100, 1_003_000, 2_000, 1_000_800));
        assert_eq!(r.duration_secs(), Some(360));

        // Once the pipeline records Voyage as a provider (§7.6) it is counted
        // there, not twice, and its tokens still stay out of the LLM aggregate.
        r.provider_costs.insert(
            VOYAGE_PROVIDER.into(),
            ProviderUsage {
                usage: usage(250_000, 0, 0, 0),
                cost_usd: 0.005,
            },
        );
        r.finish(ts("2026-08-15T05:36:00Z"));
        assert!((r.cost_usd - 0.4778).abs() < 1e-9);
        assert_eq!(r.usage, usage(1_000_100, 1_003_000, 2_000, 1_000_800));
    }

    #[test]
    fn info_block_has_the_four_lines_of_the_plan() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.counts.articles = 412;
        r.counts.eligible = 398;
        r.counts.triaged = 398;
        r.counts.assessed = 120;
        r.counts.shortlisted = 60;
        r.counts.selected = 17;
        r.counts.admitted_by = BTreeMap::from([
            ("triage".to_string(), 60),
            ("interest".to_string(), 20),
            ("knn".to_string(), 12),
            ("exploration".to_string(), 5),
            ("blend".to_string(), 23),
        ]);
        r.counts.rated_with_embeddings = 14;
        r.counts.knn_gate = 0.35;
        r.counts.verdicts_in_prompt = 41;
        for (provider, cost) in [("deepseek", 0.11), ("anthropic", 0.62), ("voyage", 0.02)] {
            r.provider_costs.insert(
                provider.into(),
                ProviderUsage {
                    usage: usage(1, 0, 0, 1),
                    cost_usd: cost,
                },
            );
        }
        r.finish(ts("2026-08-15T05:53:12Z"));
        let [curation, admission, preference, providers] = r.info_block();
        assert_eq!(
            curation,
            "curation: 412 considered → 398 eligible → 398 triaged → 120 assessed → 60 shortlisted → 17 selected"
        );
        assert_eq!(
            admission,
            "admission: triage 60 · interest 20 · knn 12 · exploration 5 · blend 23 · auto 0"
        );
        assert_eq!(
            preference,
            "preference: 14 rated w/ embeddings → knn 0.35 · feed off · 41 verdicts in prompt"
        );
        assert_eq!(
            providers,
            "providers: anthropic $0.62 · deepseek $0.11 · voyage $0.02 · total $0.75 · 23m12s"
        );
        assert_eq!(RunReport::format_duration(48), "48s");
        assert_eq!(RunReport::format_duration(3600), "60m00s");
    }

    #[test]
    fn warnings_degrade_the_run() {
        let mut r = RunReport::new("2026-08-15".parse().unwrap(), ts("2026-08-15T05:30:00Z"));
        r.warn("xtc converter missing");
        r.finish(ts("2026-08-15T05:31:00Z"));
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
        r.config_json = serde_json::json!({"models": {"editor": "claude-opus-5"}});
        r.provider_costs.insert(
            "anthropic".into(),
            ProviderUsage {
                usage: usage(1, 2, 3, 4),
                cost_usd: 0.01,
            },
        );
        r.finish(ts("2026-08-15T05:31:00Z"));
        let json = r.to_json();
        let back: RunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.top_feeds(1), vec![("Hacker News", 30)]);
        assert_eq!(back.timings.total_ms(), 1500);
        assert!(back.summary_line().contains("412 entries"));
        // `ProviderUsage` flattens the token counts next to the cost.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["provider_costs"]["anthropic"]["cache_write_tokens"],
            3
        );
        assert_eq!(value["provider_costs"]["anthropic"]["cost_usd"], 0.01);
    }
}
