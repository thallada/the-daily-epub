//! The `generate` pipeline, wired end to end (spec §2, §3.6 wiring).
//!
//! ```text
//! Miniflux ingest ─▶ dedupe ─▶ extraction ─▶ persist ─▶ social enrichment
//!   ─▶ pre-filter ─▶ LLM scoring ─▶ selection ─▶ comments ─▶ editorial
//!   ─▶ world briefing ─▶ EPUB (standard + X4) ─▶ XTC ─▶ publish ─▶ report
//! ```
//!
//! Failure policy (notes §3):
//!
//! * **Fatal** — Miniflux ingest, SQLite writes, EPUB assembly, publishing. Without
//!   any one of them there is no issue, so the run fails loudly and the `runs` row
//!   records why.
//! * **Best effort** — social enrichment, comments, the world briefing, images and
//!   the XTC conversion. They log, add a warning to the report (status `degraded`)
//!   and the run continues.
//! * **Degrading** — every DeepSeek stage. A missing key, a dead API or a tripped
//!   `max_daily_usd` guardrail turns the run into the `--skip-llm` shape
//!   (prefilter order selects, feed excerpts stand in for summaries) rather than
//!   losing the day's issue.
//!
//! The run is idempotent per date (notes §12): entries, articles, scores and the
//! issue itself are upserted, `issue_articles` is replaced wholesale, and the
//! published filenames are derived from the date.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::civil::Date;
use jiff::{Timestamp, Zoned};

use crate::config::Config;
use crate::curate::llm::{Llms, PriceTable, UsageMeter};
use crate::curate::{Curator, editorial, profile};
use crate::db::Db;
use crate::extract::Extractor;
use crate::miniflux::MinifluxClient;
use crate::publish::Published;
use crate::report::{ProviderUsage, RunReport, RunStatus};
use crate::types::{
    Article, Artifact, Colophon, Edition, Issue, IssueMeta, Lineup, Models, reading_minutes,
};
use crate::{comments, dedupe, epub, http, miniflux, publish, social, world};

/// One `generate` invocation's inputs — the CLI flags, already parsed (§2).
#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    /// `--date YYYY-MM-DD`; `None` means today in the configured timezone.
    pub date: Option<String>,
    /// `--dry-run`: build everything, publish nothing, record no issue.
    pub dry_run: bool,
    /// `--out DIR`, overriding `out_dir`.
    pub out: Option<PathBuf>,
    /// `--max-articles N`, overriding `target_article_count`.
    pub max_articles: Option<usize>,
    /// `--skip-llm`: no DeepSeek call at all.
    pub skip_llm: bool,
}

/// What one run produced, for the caller to print (§3.13).
#[derive(Debug)]
pub struct GenerateOutcome {
    pub report: RunReport,
    /// `None` only when the run failed before assembly.
    pub issue: Option<Issue>,
    /// The EPUBs as written into the output directory.
    pub artifacts: Vec<Artifact>,
    /// The converted XTC artifact, when the converter ran (§3.11).
    pub xtc: Option<PathBuf>,
    /// `None` under `--dry-run`.
    pub published: Option<Published>,
}

/// Resolve `--date` (or today) in the configured timezone (notes §2).
pub fn resolve_date(config: &Config, raw: Option<&str>) -> Result<Date> {
    let tz = config.tz()?;
    match raw {
        Some(s) => s
            .parse::<Date>()
            .with_context(|| format!("invalid --date {s:?}, expected YYYY-MM-DD")),
        None => Ok(Zoned::now().with_time_zone(tz).date()),
    }
}

/// The ingest window `[end - lookback_hours, end]` where `end` is the end of the
/// issue's day in the configured timezone, clamped to now (§3.1).
pub fn ingest_window(config: &Config, date: Date) -> Result<(Timestamp, Timestamp)> {
    let tz = config.tz()?;
    let now = Timestamp::now();
    let end_of_day = date
        .to_zoned(tz)
        .context("resolving issue date in the configured timezone")?
        .tomorrow()
        .context("computing the end of the issue day")?
        .timestamp();
    let end = end_of_day.min(now);
    let start = end - jiff::Span::new().hours(i64::from(config.lookback_hours));
    Ok((start, end))
}

/// "Friday, August 15, 2026" — the cover/front-page dateline (§3.10).
pub fn display_date(date: Date) -> String {
    const WEEKDAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let weekday = WEEKDAYS[(date.weekday().to_monday_zero_offset() as usize).min(6)];
    let month = MONTHS[(date.month() as usize).clamp(1, 12) - 1];
    format!("{weekday}, {month} {}, {}", date.day(), date.year())
}

/// Materialize the [`Issue`] the EPUB builder consumes (§3.10).
///
/// Pure: every count is derived from the lineup, so the same inputs always give
/// the same cover and stats line (notes §12).
pub fn build_issue(
    date: Date,
    issue_number: i64,
    generated_at: Timestamp,
    lineup: Lineup,
    editorial: crate::types::Editorial,
    world_briefing: Option<crate::types::WorldBriefing>,
    colophon: Colophon,
) -> Issue {
    let total_words = lineup.total_words();
    let section_count = lineup.section_order.len() as i64
        + i64::from(world_briefing.is_some() && !lineup.section_order.is_empty());
    let meta = IssueMeta {
        date,
        issue_number,
        generated_at,
        display_date: display_date(date),
        article_count: lineup.picks.len() as i64,
        section_count,
        total_words,
        reading_minutes: reading_minutes(total_words),
    };
    Issue {
        meta,
        lineup,
        editorial,
        world_briefing,
        colophon,
    }
}

/// Copy stage-C summaries onto their picks so `issue_articles` and the EPUB agree.
pub fn apply_summaries(lineup: &mut Lineup, editorial: &crate::types::Editorial) {
    for pick in &mut lineup.picks {
        if pick.summary.is_none()
            && let Some(summary) = editorial.summaries.get(&pick.article.id)
        {
            pick.summary = Some(summary.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Run one issue end to end, recording a `runs` row either way (§2, §3.13).
pub async fn generate(config: &Config, db: &Db, opts: &GenerateOptions) -> Result<GenerateOutcome> {
    let date = resolve_date(config, opts.date.as_deref())?;
    let started_at = Timestamp::now();
    let (window_start, window_end) = ingest_window(config, date)?;
    let out_dir = opts.out.clone().unwrap_or_else(|| config.out_dir.clone());
    let (soft_target, hard_max) = issue_size_bounds(config, opts.max_articles);

    let span = tracing::info_span!("generate", %date, dry_run = opts.dry_run);
    let _guard = span.enter();
    tracing::info!(
        %window_start,
        %window_end,
        lookback_hours = config.lookback_hours,
        soft_target,
        hard_max,
        skip_llm = opts.skip_llm,
        out = %out_dir.display(),
        "starting run"
    );
    log_resolved_providers(config, opts.skip_llm);

    let run_id = db.start_run(date, started_at).await?;
    let mut report = RunReport::new(date, started_at);
    report.window_start = Some(window_start);
    report.window_end = Some(window_end);
    report.config_json = resolved_run_config(config, soft_target, hard_max);
    if opts.dry_run {
        report.status = RunStatus::DryRun;
    }

    let ctx = StageContext {
        config,
        db,
        date,
        soft_target,
        hard_max,
        started_at,
        out_dir,
        dry_run: opts.dry_run,
        skip_llm: opts.skip_llm,
    };
    let stages = match run_stages(&ctx, window_start, window_end, &mut report).await {
        Ok(stages) => {
            report.finish(Timestamp::now());
            stages
        }
        Err(e) => {
            report.fail(Timestamp::now(), format!("{e:#}"));
            db.finish_run(run_id, &report).await?;
            return Err(e);
        }
    };

    db.finish_run(run_id, &report).await?;
    // The issue row is written before the report is costed, so stamp the finished
    // report onto it now (the paths are preserved by `COALESCE`, §3.13).
    if !opts.dry_run
        && let Some(issue) = stages.issue.as_ref()
        && let Err(e) = db
            .upsert_issue(
                date,
                issue.meta.issue_number,
                issue.meta.generated_at,
                None,
                None,
                None,
                None,
                Some(&report.to_json()),
            )
            .await
    {
        tracing::warn!(error = %e, "could not attach the run report to the issue");
    }
    Ok(GenerateOutcome {
        report,
        issue: stages.issue,
        artifacts: stages.artifacts,
        xtc: stages.xtc,
        published: stages.published,
    })
}

/// What [`run_stages`] hands back; [`generate`] pairs it with the costed report.
#[derive(Debug)]
struct StageOutput {
    issue: Option<Issue>,
    artifacts: Vec<Artifact>,
    xtc: Option<PathBuf>,
    published: Option<Published>,
}

/// Everything the stages need that does not change between them.
struct StageContext<'a> {
    config: &'a Config,
    db: &'a Db,
    date: Date,
    soft_target: usize,
    hard_max: usize,
    started_at: Timestamp,
    out_dir: PathBuf,
    dry_run: bool,
    skip_llm: bool,
}

async fn run_stages(
    ctx: &StageContext<'_>,
    window_start: Timestamp,
    window_end: Timestamp,
    report: &mut RunReport,
) -> Result<StageOutput> {
    let (config, db, date) = (ctx.config, ctx.db, ctx.date);
    let http = http::build_client(http::DEFAULT_TIMEOUT).context("building http client")?;

    // --- Stage 1: Miniflux ingest (§3.1) — fatal on failure ---
    let stage = Timestamp::now();
    let client = MinifluxClient::new(&config.miniflux, http.clone())
        .context("constructing the miniflux client")?;
    let (entries, feeds) = client
        .ingest_window(window_start, window_end, Timestamp::now())
        .await
        .context("ingesting entries from miniflux")?;

    report.counts.entries_fetched = entries.len() as i64;
    report.counts.feeds_seen = entries
        .iter()
        .map(|e| e.feed_id)
        .collect::<BTreeSet<_>>()
        .len() as i64;
    let mut per_feed: BTreeMap<String, i64> = BTreeMap::new();
    for entry in &entries {
        let name = entry
            .feed_title
            .clone()
            .or_else(|| feeds.get(&entry.feed_id).map(|f| f.title.clone()))
            .unwrap_or_else(|| format!("feed {}", entry.feed_id));
        *per_feed.entry(name).or_insert(0) += 1;
    }
    report.per_feed_counts = per_feed;

    // Entries are persisted even on a dry run: `articles.best_entry_id` is a real
    // foreign key, and the social cache keys off the article ids. Only the
    // watermark (an ingest bookmark) is left alone.
    let written = db
        .upsert_entries(&entries)
        .await
        .context("persisting entries")?;
    if ctx.dry_run {
        tracing::info!(written, "dry run: persisted entries, watermark left alone");
    } else {
        db.set_watermark(window_end).await?;
        tracing::info!(written, "persisted entries and advanced the watermark");
    }
    report.timings.record("ingest", elapsed_ms(stage));

    // --- Stage 2: normalize + dedupe (§3.2) ---
    let stage = Timestamp::now();
    let feed_urls = miniflux::feed_urls(&feeds);
    let (mut articles, dedupe_stats) = dedupe::cluster_with_feeds(entries, &feed_urls);
    report.counts.entries_dropped = dedupe_stats.dropped_non_article as i64;
    report.counts.articles = dedupe_stats.clusters as i64;
    report.counts.duplicates_merged = dedupe_stats.merged as i64;
    report.timings.record("dedupe", elapsed_ms(stage));

    // --- Stage 3: content extraction (§3.3) — before persisting, because it
    // replaces the raw Miniflux body that `dedupe` left on the cluster ---
    let stage = Timestamp::now();
    let extractor = Extractor::new(http.clone(), config.curation.paywall_domains.clone());
    let extract_stats = extractor.extract_all(&mut articles).await;
    report.counts.extracted = (extract_stats.from_miniflux + extract_stats.from_readability) as i64;
    report.counts.excerpt_only = extract_stats.excerpt_only as i64;
    if extract_stats.fetch_failures > 0 {
        report.warn(format!(
            "{} articles fell back to a feed excerpt",
            extract_stats.fetch_failures
        ));
    }
    report.timings.record("extract", elapsed_ms(stage));

    // --- Stage 4: persist the clusters, minting real article ids (§3.13) ---
    let stage = Timestamp::now();
    persist_articles(db, &mut articles).await?;
    report.timings.record("persist", elapsed_ms(stage));

    // --- Stage 5: social enrichment (§3.4) — best effort, needs real ids ---
    let stage = Timestamp::now();
    let enricher = social::SocialEnricher::new(http.clone(), db.clone());
    report.counts.social_hits = enricher.enrich_all(&mut articles).await as i64;
    report.timings.record("social", elapsed_ms(stage));

    // --- Stage 6: heuristic pre-filter (§3.5) ---
    let stage = Timestamp::now();
    let bulk_meter =
        UsageMeter::with_prices(PriceTable::deepseek(&config.deepseek), config.max_daily_usd);
    let editor_meter = UsageMeter::with_prices(
        PriceTable::anthropic(&config.anthropic),
        config.anthropic.max_daily_usd,
    );
    match db.provider_spend_for_utc_day(ctx.started_at).await {
        Ok(spend) => {
            bulk_meter.preload_cost(spend.get("deepseek").copied().unwrap_or(0.0));
            editor_meter.preload_cost(spend.get("anthropic").copied().unwrap_or(0.0));
        }
        Err(error) => {
            tracing::warn!(%error, "could not preload provider spend; starting from zero")
        }
    }

    let llms = build_llms(ctx, &bulk_meter, &editor_meter, report).await;
    let bulk_available = llms.bulk.is_some();
    let mut curator_config = config.clone();
    curator_config.target_article_count = ctx.soft_target;
    curator_config.curation.max_article_count = ctx.hard_max;
    let curator = Curator::new(curator_config, db.clone(), llms);

    let mut candidates = curator
        .prefilter(articles, date)
        .await
        .context("running the heuristic pre-filter")?;
    report.counts.candidates = candidates.len() as i64;
    report.timings.record("prefilter", elapsed_ms(stage));

    // --- Stage 7: LLM scoring, then selection (§3.6 A + B) ---
    let stage = Timestamp::now();
    if bulk_available && let Err(e) = curator.score(&mut candidates, date).await {
        // A dead API or a tripped budget must not cost us the issue: selection
        // degrades to prefilter order exactly as `--skip-llm` does.
        report.warn(format!("LLM scoring failed; ranking heuristically: {e:#}"));
    }
    report.counts.llm_scored = candidates.iter().filter(|c| c.llm.is_some()).count() as i64;
    report.counts.llm_unscored = report.counts.candidates - report.counts.llm_scored;

    let mut lineup = curator
        .select(candidates, date)
        .await
        .context("selecting the lineup")?;
    report.counts.selected = lineup.picks.len() as i64;
    if lineup.picks.is_empty() {
        report.warn("the lineup is empty — check the lookback window and pre-filter");
    }
    report.timings.record("curate", elapsed_ms(stage));

    // --- Stage 8: comment chapters for the selected articles (§3.7) ---
    let stage = Timestamp::now();
    report.counts.discussions = comments::fetch_all(&http, &mut lineup.picks).await as i64;
    report.timings.record("comments", elapsed_ms(stage));

    // --- Stage 9: editorial (§3.6 C) ---
    let stage = Timestamp::now();
    let editorial = match curator.editorial(&lineup).await {
        Ok(editorial) => editorial,
        Err(e) => {
            report.warn(format!(
                "editorial generation failed; using excerpts: {e:#}"
            ));
            editorial::fallback_editorial(&lineup)
        }
    };
    apply_summaries(&mut lineup, &editorial);
    report.timings.record("editorial", elapsed_ms(stage));

    // --- Stage 10: completed-day World Briefing (§3.8), best effort ---
    // Editorial retains budget priority; only the remaining metered budget is
    // available for per-event summaries and the overview.
    let stage = Timestamp::now();
    let mut world_briefing = world::fetch_optional(&http, date, config.world_briefing).await;
    if config.world_briefing {
        match world_briefing.as_mut() {
            Some(briefing) => {
                for warning in world::enrich(&http, briefing, curator.llms.bulk.as_ref()).await {
                    report.warn(warning);
                }
            }
            None => report.warn("the world briefing was unavailable; the section is omitted"),
        }
    }
    report.timings.record("world", elapsed_ms(stage));

    // --- Stage 11: assemble the issue (§3.10) ---
    let issue_number = db
        .next_issue_number(date)
        .await
        .context("computing the issue number")?;
    report.provider_costs.insert(
        "deepseek".into(),
        ProviderUsage {
            usage: bulk_meter.total(),
            cost_usd: bulk_meter.cost_usd(),
        },
    );
    report.provider_costs.insert(
        "anthropic".into(),
        ProviderUsage {
            usage: editor_meter.total(),
            cost_usd: editor_meter.cost_usd(),
        },
    );
    let summary_model = match config.editorial.summary_model {
        crate::config::SummaryModel::Editor if curator.llms.editor.is_some() => {
            config.anthropic.model.clone()
        }
        _ if curator.llms.bulk.is_some() => config.deepseek.model.clone(),
        _ => "none".into(),
    };
    let provider_costs = report
        .provider_costs
        .iter()
        .map(|(provider, usage)| (provider.clone(), usage.cost_usd))
        .collect();
    let colophon = Colophon {
        provider_costs,
        models: Models {
            bulk: if bulk_available {
                config.deepseek.model.clone()
            } else {
                "none".into()
            },
            editor: if curator.llms.editor.is_some() {
                config.anthropic.model.clone()
            } else if bulk_available {
                format!("{} (bulk fallback)", config.deepseek.model)
            } else {
                "none".into()
            },
            summaries: summary_model,
        },
        entries_fetched: report.counts.entries_fetched,
        feeds_seen: report.counts.feeds_seen,
        candidates: report.counts.candidates,
        cost_usd: bulk_meter.cost_usd() + editor_meter.cost_usd(),
        generator_version: format!("daily-epub {}", crate::VERSION),
    };
    let issue = build_issue(
        date,
        issue_number,
        Timestamp::now(),
        lineup,
        editorial,
        world_briefing,
        colophon,
    );

    // --- Stage 12: build both EPUB editions (§3.10) — fatal on failure ---
    let stage = Timestamp::now();
    let (artifacts, images) = epub::build_all(&issue, config, &ctx.out_dir)
        .await
        .context("building the EPUB editions")?;
    report.counts.images_embedded = images as i64;
    report.timings.record("epub", elapsed_ms(stage));

    // --- Stage 13: XTC conversion (§3.11) — best effort ---
    let stage = Timestamp::now();
    let x4 = artifacts.iter().find(|a| a.edition == Edition::X4);
    let xtc = match x4 {
        Some(artifact) if config.xtc.enabled => {
            match epub::x4::convert(&config.xtc, &artifact.path, &ctx.out_dir).await {
                Ok(path) => Some(path),
                Err(e) => {
                    // Carry the converter's own message into the report: "did not
                    // produce a file" alone cannot distinguish a missing Node from
                    // a missing settings file from a genuine conversion failure.
                    tracing::warn!("xtc conversion skipped: {e}");
                    report.warn(format!("the XTC conversion did not produce a file: {e}"));
                    None
                }
            }
        }
        _ => None,
    };
    report.timings.record("xtc", elapsed_ms(stage));

    // --- Stage 14: publish + record the issue (§3.11) ---
    let stage = Timestamp::now();
    let published = if ctx.dry_run {
        tracing::info!(
            out = %ctx.out_dir.display(),
            "dry run: skipping BookOrbit/XTC publishing and the issue record"
        );
        None
    } else {
        let published = publish::publish_issue(config, &issue, &artifacts, xtc.as_deref())
            .await
            .context("publishing the issue")?;
        record_issue(db, &issue, &published)
            .await
            .context("recording the issue")?;
        Some(published)
    };
    report.timings.record("publish", elapsed_ms(stage));

    Ok(StageOutput {
        issue: Some(issue),
        artifacts,
        xtc,
        published,
    })
}

/// Insert/refresh the `articles` rows and stamp the returned ids back on (§3.13).
async fn persist_articles(db: &Db, articles: &mut [Article]) -> Result<()> {
    for article in articles.iter_mut() {
        let id = db
            .upsert_article(article)
            .await
            .with_context(|| format!("persisting article {}", article.canonical_url))?;
        article.id = id;
        for social_ref in &mut article.social {
            social_ref.article_id = id;
        }
    }
    tracing::info!(articles = articles.len(), "persisted article clusters");
    Ok(())
}

/// Write the `issues` row and replace `issue_articles` for the date (notes §12).
async fn record_issue(db: &Db, issue: &Issue, published: &Published) -> Result<()> {
    let path_for = |edition: Edition| {
        published
            .epubs
            .iter()
            .find(|a| a.edition == edition)
            .map(|a| a.path.display().to_string())
    };
    let epub_path = path_for(Edition::Standard);
    let x4_path = path_for(Edition::X4);
    let xtc_path = published.xtc.as_ref().map(|p| p.display().to_string());

    db.upsert_issue(
        issue.meta.date,
        issue.meta.issue_number,
        issue.meta.generated_at,
        epub_path.as_deref(),
        x4_path.as_deref(),
        xtc_path.as_deref(),
        Some(&issue.editorial.front_page_html),
        None,
    )
    .await?;
    db.replace_issue_articles(issue.meta.date, &issue.lineup.picks)
        .await?;
    Ok(())
}

/// Build the DeepSeek client, running the weekly profile rebuild when it is due.
///
/// Returns `None` for `--skip-llm` and for every configuration/API problem: the
/// caller then curates heuristically instead of failing the run (§3.6).
async fn build_llms(
    ctx: &StageContext<'_>,
    bulk_meter: &UsageMeter,
    editor_meter: &UsageMeter,
    report: &mut RunReport,
) -> Llms {
    let profile = match profile::load_or_build(
        ctx.db,
        &ctx.config.interests_opml,
        &ctx.config.profile_path,
        ctx.config.curation.feedback.verdicts_in_prompt,
    )
    .await
    {
        Ok(profile) => profile,
        Err(error) => {
            report.warn(format!(
                "could not build the taste profile; curating heuristically: {error:#}"
            ));
            return Llms::default();
        }
    };
    if ctx.skip_llm {
        tracing::info!("--skip-llm: profile rebuilt; no provider calls will be made");
        return Llms::default();
    }

    let make_clients = |prompt: String| {
        Llms::from_config(
            &ctx.config.deepseek,
            &ctx.config.anthropic,
            prompt,
            bulk_meter.clone(),
            editor_meter.clone(),
        )
    };

    let mut llms = make_clients(profile.text);
    let Some(rebuild_client) = llms.editor_or_bulk() else {
        report.warn("no LLM provider is available; curating heuristically");
        return llms;
    };
    match profile::weekly_rebuild_if_due(
        ctx.db,
        rebuild_client,
        &ctx.config.interests_opml,
        &ctx.config.profile_path,
        ctx.config.curation.feedback.verdicts_in_prompt,
    )
    .await
    {
        Ok(Some(rebuilt)) => {
            tracing::info!(
                version = rebuilt.version,
                "taste profile rebuilt with editor-or-bulk"
            );
            llms = make_clients(rebuilt.text);
        }
        Ok(None) => {}
        Err(error) => report.warn(format!("weekly profile rebuild failed: {error:#}")),
    }
    llms
}

/// `--max-articles N` is a ceiling, never a target (§13): the hard ceiling is
/// the smaller of `curation.max_article_count` and `N`, and the soft target
/// never exceeds it. Returns `(soft_target, hard_max)`.
pub fn issue_size_bounds(config: &Config, max_articles: Option<usize>) -> (usize, usize) {
    let hard_max = max_articles.map_or(config.curation.max_article_count, |ceiling| {
        ceiling.min(config.curation.max_article_count)
    });
    (config.target_article_count.min(hard_max), hard_max)
}

/// Startup line naming the resolved models and whether each provider is on
/// (§19): the root config ignores unknown sections, so an `[anthropics]` typo
/// would otherwise be silent. Keys are never logged, only their presence.
fn log_resolved_providers(config: &Config, skip_llm: bool) {
    let has_key = |key: Option<&str>| key.is_some_and(|k| !k.trim().is_empty());
    tracing::info!(
        bulk_model = %config.deepseek.model,
        bulk_enabled = !skip_llm && has_key(config.deepseek.api_key.as_deref()),
        bulk_max_daily_usd = config.max_daily_usd,
        editor_model = %config.anthropic.model,
        editor_enabled = !skip_llm
            && config.anthropic.enabled
            && has_key(config.anthropic.api_key.as_deref()),
        editor_effort = %config.anthropic.effort,
        editor_max_daily_usd = config.anthropic.max_daily_usd,
        summary_model = ?config.editorial.summary_model,
        "resolved providers"
    );
}

/// Prompt versions recorded per run so old telemetry stays interpretable (§7.6).
/// Bump a number when the corresponding instruction block changes.
const PROMPT_VERSIONS: &[(&str, u32)] = &[
    ("score", 1),
    ("editor", 2),
    ("summary", 1),
    ("brief", 2),
    ("profile", 2),
];

/// The resolved `[curation]`, `[editorial]`, model names and prompt versions
/// written to `runs.config_json` (§7.6, §19). Never includes keys.
fn resolved_run_config(config: &Config, soft_target: usize, hard_max: usize) -> serde_json::Value {
    let mut curation = config.curation.clone();
    curation.max_article_count = hard_max;
    serde_json::json!({
        "target_article_count": soft_target,
        "prefilter_keep": config.prefilter_keep,
        "curation": curation,
        "editorial": config.editorial,
        "models": {
            "bulk": config.deepseek.model,
            "editor": if config.anthropic.enabled { config.anthropic.model.as_str() } else { "disabled" },
            "editor_effort": config.anthropic.effort,
        },
        "prompt_versions": PROMPT_VERSIONS
            .iter()
            .map(|(name, version)| ((*name).to_string(), serde_json::Value::from(*version)))
            .collect::<serde_json::Map<_, _>>(),
    })
}

fn elapsed_ms(since: Timestamp) -> i64 {
    (Timestamp::now().as_millisecond() - since.as_millisecond()).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_date_matches_the_masthead_format() {
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(display_date(date), "Saturday, August 15, 2026");
        let date: Date = "2026-01-01".parse().unwrap();
        assert_eq!(display_date(date), "Thursday, January 1, 2026");
        let date: Date = "2026-12-31".parse().unwrap();
        assert_eq!(display_date(date), "Thursday, December 31, 2026");
    }

    #[test]
    fn ingest_window_spans_the_lookback() {
        let config = Config::default();
        let date: Date = "2020-01-15".parse().unwrap();
        let (start, end) = ingest_window(&config, date).unwrap();
        assert!(start < end);
        let hours = (end.as_second() - start.as_second()) / 3600;
        assert_eq!(hours, i64::from(config.lookback_hours));
        // 2020-01-16T00:00 America/New_York == 2020-01-16T05:00Z
        assert_eq!(end.to_string(), "2020-01-16T05:00:00Z");
    }

    #[test]
    fn resolve_date_parses_and_defaults() {
        let config = Config::default();
        assert_eq!(
            resolve_date(&config, Some("2026-08-15"))
                .unwrap()
                .to_string(),
            "2026-08-15"
        );
        assert!(resolve_date(&config, Some("nope")).is_err());
        assert!(resolve_date(&config, None).is_ok());
    }

    #[test]
    fn max_articles_is_a_ceiling_not_a_target() {
        let config = Config {
            target_article_count: 20,
            ..Config::default()
        };
        assert_eq!(config.curation.max_article_count, 28);
        assert_eq!(issue_size_bounds(&config, None), (20, 28));
        // A ceiling below the target drags the target down with it.
        assert_eq!(issue_size_bounds(&config, Some(6)), (6, 6));
        // A ceiling above the configured maximum does not raise it.
        assert_eq!(issue_size_bounds(&config, Some(40)), (20, 28));
        assert_eq!(issue_size_bounds(&config, Some(24)), (20, 24));
    }

    #[test]
    fn run_config_json_records_the_resolved_settings_and_no_keys() {
        let mut config = Config::default();
        config.anthropic.api_key = Some("sk-secret".into());
        config.deepseek.api_key = Some("ds-secret".into());
        let value = resolved_run_config(&config, 6, 6);
        assert_eq!(value["target_article_count"], 6);
        assert_eq!(value["curation"]["max_article_count"], 6);
        assert_eq!(value["editorial"]["summary_model"], "editor");
        assert_eq!(value["editorial"]["summary_input_tokens"], 3000);
        assert_eq!(value["models"]["bulk"], "deepseek-v4-flash");
        assert_eq!(value["models"]["editor"], "claude-opus-5");
        assert!(value["prompt_versions"]["editor"].is_number());
        let text = value.to_string();
        assert!(
            !text.contains("secret"),
            "keys must never reach the database"
        );
    }

    #[test]
    fn issue_meta_is_derived_from_the_lineup() {
        let lineup = crate::epub::build::fixtures::issue().lineup;
        let words = lineup.total_words();
        let sections = lineup.section_order.len() as i64;
        let issue = build_issue(
            "2026-08-15".parse().unwrap(),
            7,
            "2026-08-15T09:30:00Z".parse().unwrap(),
            lineup,
            crate::types::Editorial::default(),
            None,
            Colophon::default(),
        );
        assert_eq!(issue.meta.issue_number, 7);
        assert_eq!(issue.meta.display_date, "Saturday, August 15, 2026");
        assert_eq!(issue.meta.article_count, issue.lineup.picks.len() as i64);
        assert_eq!(issue.meta.section_count, sections);
        assert_eq!(issue.meta.total_words, words);
        assert_eq!(issue.meta.reading_minutes, reading_minutes(words));
    }

    #[test]
    fn summaries_land_on_their_picks() {
        let mut lineup = crate::epub::build::fixtures::issue().lineup;
        for pick in &mut lineup.picks {
            pick.summary = None;
        }
        let mut editorial = crate::types::Editorial::default();
        let first = lineup.picks[0].article.id;
        editorial.summaries.insert(first, "An abstract.".into());
        apply_summaries(&mut lineup, &editorial);
        assert_eq!(lineup.picks[0].summary.as_deref(), Some("An abstract."));
        assert!(lineup.picks[1..].iter().all(|p| p.summary.is_none()));
    }
}
