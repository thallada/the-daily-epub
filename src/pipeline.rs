//! The `generate` pipeline, wired end to end (spec §2, §3.6 wiring).
//!
//! ```text
//! Miniflux ingest ─▶ dedupe ─▶ extraction ─▶ persist ─▶ social enrichment
//!   ─▶ hygiene ─▶ embeddings + signals ─▶ triage ─▶ admission ─▶ deep assessment
//!   ─▶ utility + shortlist ─▶ editor ─▶ comments ─▶ summaries + brief
//!   ─▶ world briefing ─▶ behind the paper ─▶ EPUB (standard + X4) ─▶ XTC
//!   ─▶ publish ─▶ report
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
//! * **Degrading** — every LLM stage. A missing key, a dead API or a tripped
//!   provider `max_daily_usd` guardrail turns the run into the `--skip-llm` shape
//!   (cheap-signal admission, feed excerpts as summaries) rather than
//!   losing the day's issue.
//!
//! The run is idempotent per date (notes §12): entries, articles, assessments and the
//! issue itself are upserted, `issue_articles` is replaced wholesale, and the
//! published filenames are derived from the date.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::civil::Date;
use jiff::{Timestamp, Zoned};

use crate::config::Config;
use crate::curate::llm::{Llms, UsageMeter, provider_meters};
use crate::curate::{
    Curator, admit, editorial, embedding, profile, rank, signals, telemetry, triage,
};
use crate::db::Db;
use crate::extract::Extractor;
use crate::miniflux::MinifluxClient;
use crate::publish::Published;
use crate::report::{ProviderUsage, RunReport, RunStatus, VOYAGE_PROVIDER};
use crate::types::{
    Article, ArticleId, Artifact, BehindThePaper, Candidate, Colophon, Edition, Issue, IssueMeta,
    Lineup, Models, TokenUsage, reading_minutes,
};
use crate::{comments, dedupe, discovery, epub, http, miniflux, publish, social, world};

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
    /// `--skip-llm`: no chat-provider call at all.
    pub skip_llm: bool,
    /// `--skip-embeddings`: read the cache but make zero Voyage calls.
    pub skip_embeddings: bool,
    /// Ignore reusable triage/deep assessments.
    pub rescore: bool,
}

/// What one run produced, for the caller to print (§3.13).
#[derive(Debug)]
pub struct GenerateOutcome {
    pub run_id: i64,
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

/// "Fri, Aug 15" — the same dateline abbreviated for narrow screens (web only).
pub fn short_display_date(date: Date) -> String {
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = WEEKDAYS[(date.weekday().to_monday_zero_offset() as usize).min(6)];
    let month = MONTHS[(date.month() as usize).clamp(1, 12) - 1];
    format!("{weekday}, {month} {}", date.day())
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
        behind: BehindThePaper::default(),
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
        skip_embeddings = opts.skip_embeddings,
        rescore = opts.rescore,
        voyage_enabled = config.voyage.enabled,
        out = %out_dir.display(),
        "starting run"
    );
    log_resolved_providers(config, opts.skip_llm, opts.skip_embeddings);

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
        run_id,
        date,
        soft_target,
        hard_max,
        started_at,
        out_dir,
        dry_run: opts.dry_run,
        skip_llm: opts.skip_llm,
        skip_embeddings: opts.skip_embeddings,
        rescore: opts.rescore,
    };
    let stages = match run_stages(&ctx, window_start, window_end, &mut report).await {
        Ok(stages) => {
            report.finish(Timestamp::now());
            // The once-per-run info block of §15.4.
            for line in report.info_block() {
                tracing::info!("{line}");
            }
            stages
        }
        Err(e) => {
            report.fail(Timestamp::now(), format!("{e:#}"));
            db.finish_run(run_id, &report).await?;
            return Err(e);
        }
    };

    db.finish_run(run_id, &report).await?;
    if stages.published.is_some() {
        prune_retention(config, db).await;
    }
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
                None,
            )
            .await
    {
        tracing::warn!(error = %e, "could not attach the run report to the issue");
    }
    Ok(GenerateOutcome {
        run_id,
        report,
        issue: stages.issue,
        artifacts: stages.artifacts,
        xtc: stages.xtc,
        published: stages.published,
    })
}

/// The retention sweep of `features prune` (§7.1, §7.4), run once per
/// published issue. Best effort: a failure is logged and never touches the run.
async fn prune_retention(config: &Config, db: &Db) {
    let ranking = &config.curation.ranking;
    match telemetry::prune(
        db,
        ranking.embedding_retention_days,
        ranking.telemetry_retention_days,
        Timestamp::now(),
    )
    .await
    {
        Ok(pruned) => tracing::info!(
            embeddings = pruned.embeddings,
            candidate_rows = pruned.telemetry,
            assessments = pruned.assessments,
            "retention prune complete"
        ),
        Err(error) => tracing::warn!(%error, "retention prune failed; continuing"),
    }
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
    run_id: i64,
    date: Date,
    soft_target: usize,
    hard_max: usize,
    started_at: Timestamp,
    out_dir: PathBuf,
    dry_run: bool,
    skip_llm: bool,
    skip_embeddings: bool,
    rescore: bool,
}

/// Share of the day's articles that must fall back to a feed excerpt before the
/// run is degraded over it. Single-digit percentages are routine; the exact count
/// is always in `counts.excerpt_only`.
const EXCERPT_FALLBACK_WARN_SHARE: f64 = 0.30;

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
    // A handful of fetch failures is the normal state of the open web, so only a
    // day well past the usual rate is worth degrading the run over.
    if extract_stats.fetch_failures > 0 {
        let share = extract_stats.fetch_failures as f64 / articles.len().max(1) as f64;
        if share >= EXCERPT_FALLBACK_WARN_SHARE {
            report.warn(format!(
                "{} of {} articles ({:.0}%) fell back to a feed excerpt",
                extract_stats.fetch_failures,
                articles.len(),
                share * 100.0
            ));
        } else {
            tracing::info!(
                failures = extract_stats.fetch_failures,
                articles = articles.len(),
                "articles fell back to a feed excerpt"
            );
        }
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

    // --- Stage 5b: feed discovery (feed discovery plan §4) — best effort ---
    // Runs here because it needs the real article ids stage 4 minted and the
    // subscription map stage 1 already loaded, and because `articles` is moved
    // into hygiene next.
    if config.discovery.enabled {
        let stage = Timestamp::now();
        match discovery::run(
            db,
            &client,
            &http,
            &config.discovery,
            &articles,
            &feeds,
            Timestamp::now(),
        )
        .await
        {
            Ok(summary) => {
                tracing::info!(%summary, "feed discovery");
                report.counts.feed_candidates_new = summary.candidates_new as i64;
            }
            Err(error) => report.warn(format!("feed discovery failed: {error:#}")),
        }
        report.timings.record("discovery", elapsed_ms(stage));
    }

    // --- Stage 6: hygiene, embeddings, and cheap signals (§8.1, §9) ---
    let stage = Timestamp::now();
    let mut personalized = admit::hygiene(
        db,
        ctx.run_id,
        articles,
        date,
        &config.curation,
        ctx.started_at,
    )
    .await
    .context("running candidate hygiene")?;
    report.counts.eligible = personalized.len() as i64;
    report.timings.record("hygiene", elapsed_ms(stage));
    let embeddings = build_embedding_service(ctx, report);
    let article_embeddings = prepare_features(ctx, &mut personalized, &embeddings, report).await;

    // Build the provider clients before triage. A missing or failed bulk client
    // skips triage and deep assessment, while the editor can still run (§17).
    // One meter per referenced provider, keyed by its `[providers.*]` name and
    // preloaded with what earlier runs on this UTC day already spent on it.
    let stage = Timestamp::now();
    let meters = provider_meters(config);
    match db.provider_spend_for_utc_day(ctx.started_at).await {
        Ok(spend) => {
            for (name, meter) in &meters {
                meter.preload_cost(spend.get(name).copied().unwrap_or(0.0));
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not preload provider spend; starting from zero")
        }
    }

    let llms = build_llms(ctx, &meters, report).await;
    let mut curator_config = config.clone();
    curator_config.target_article_count = ctx.soft_target;
    curator_config.curation.max_article_count = ctx.hard_max;
    let curator = Curator::new(curator_config, db.clone(), llms);

    report.timings.record("providers", elapsed_ms(stage));

    // --- Stage 7: triage (§10) ---
    let stage = Timestamp::now();
    let triage_pool = triage::apply_pool_cap(&mut personalized, config.curation.ranking.triage_max);
    let profile_version = db
        .kv_get(crate::db::KV_PROFILE_VERSION)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok());
    if let Some(bulk) = curator.llms.bulk.as_ref() {
        match triage::run(
            db,
            bulk,
            curator.llms.editor.as_ref(),
            &mut personalized,
            &triage_pool,
            config.llm.triage_batch_size,
            bulk.max_concurrent_requests,
            config.curation.ranking.assessment_reuse_days,
            ctx.rescore,
            profile_version,
            Timestamp::now(),
            config.llm.score_temperature,
        )
        .await
        {
            Ok(summary) => {
                report.counts.triage_reused = summary.reused as i64;
                report.counts.triage_rejected = summary.rejected_total() as i64;
            }
            Err(error) => report.warn(format!(
                "triage degraded; admission continues without it: {error:#}"
            )),
        }
    } else {
        tracing::info!("--skip-llm or no bulk provider: triage skipped");
    }
    report.counts.triaged = personalized
        .iter()
        .filter(|candidate| candidate.assessment.triage.is_some())
        .count() as i64;
    report.timings.record("triage", elapsed_ms(stage));

    // --- Stage 8: union admission (§11) ---
    let stage = Timestamp::now();
    let admission = admit::admit(&mut personalized, date, &config.curation.ranking);
    report.counts.admitted = admission.admitted as i64;
    report.counts.candidates = admission.admitted as i64;
    report.counts.admitted_by = admission
        .admitted_by
        .iter()
        .map(|(name, count)| (name.clone(), *count as i64))
        .collect();
    report.counts.exploration_admitted = admission.exploration_admitted as i64;
    record_candidates(ctx, &personalized)
        .await
        .context("recording admission telemetry")?;
    tracing::debug!(admitted_by = ?admission.admitted_by, "admission complete");
    report.timings.record("admit", elapsed_ms(stage));

    // --- Stage 9: deep assessment (§12.1) ---
    let stage = Timestamp::now();
    match curator
        .assess(
            &mut personalized,
            ctx.rescore,
            profile_version,
            Timestamp::now(),
        )
        .await
    {
        Ok(summary) => {
            report.counts.deep_reused = summary.reused as i64;
            report.counts.deep_rejected = summary.rejected_total() as i64;
        }
        Err(error) => report.warn(format!(
            "deep assessment degraded; ranking continues on present signals: {error:#}"
        )),
    }
    report.counts.assessed = personalized
        .iter()
        .filter(|candidate| candidate.assessment.deep.is_some())
        .count() as i64;
    record_candidates(ctx, &personalized)
        .await
        .context("recording assessment telemetry")?;
    report.timings.record("assess", elapsed_ms(stage));

    // --- Stage 10: utility and diversified shortlist (§12.2–§12.5) ---
    let stage = Timestamp::now();
    let ranked = rank::shortlist(
        &mut personalized,
        &article_embeddings,
        &config.curation.ranking,
    );
    report.counts.shortlisted = ranked.shortlisted as i64;
    report.counts.clusters = ranked.clusters as i64;
    record_candidates(ctx, &personalized)
        .await
        .context("recording ranking telemetry")?;
    report.timings.record("rank", elapsed_ms(stage));

    let mut candidates = personalized
        .iter()
        .filter(|candidate| candidate.stage == "shortlisted")
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.rank_utility.unwrap_or(i64::MAX));
    let shortlisted = candidates
        .iter()
        .map(|candidate| candidate.article.id)
        .collect::<Vec<_>>();

    // --- Stage 11: editor (§13) ---
    let stage = Timestamp::now();
    let mut lineup = curator
        .select(candidates, date)
        .await
        .context("selecting the lineup")?;
    report.counts.selected = lineup.picks.len() as i64;
    let selected = lineup
        .picks
        .iter()
        .map(|pick| pick.article.id)
        .collect::<Vec<_>>();
    let selected_set = selected.iter().copied().collect::<HashSet<_>>();
    let not_selected = shortlisted
        .iter()
        .copied()
        .filter(|id| !selected_set.contains(id))
        .collect::<Vec<_>>();
    // The editor's one-line `why` (§13) lands in `candidate_runs.editor_why` so
    // `explain` can quote it; heuristic picks leave it NULL.
    let selected_with_why = lineup
        .picks
        .iter()
        .map(|pick| (pick.article.id, pick.why.as_deref()))
        .collect::<Vec<_>>();
    set_candidate_stage(
        &mut personalized,
        &not_selected,
        "shortlisted",
        Some("not_selected"),
    );
    let why = selected_with_why.into_iter().collect::<HashMap<_, _>>();
    for candidate in &mut personalized {
        if selected_set.contains(&candidate.article.id) {
            candidate.stage = "selected".into();
            candidate.excluded_reason = None;
        }
    }
    record_candidates_with_why(ctx, &personalized, &why)
        .await
        .context("recording selection telemetry")?;
    report.counts.exploration_selected = personalized
        .iter()
        .filter(|candidate| candidate.stage == "selected" && candidate.exploration)
        .count() as i64;
    if lineup.picks.is_empty() {
        report.warn("the lineup is empty — check the lookback window and admission settings");
    }
    report.timings.record("editor", elapsed_ms(stage));

    // --- Stage 8: comment chapters for the selected articles (§3.7) ---
    let stage = Timestamp::now();
    report.counts.discussions = comments::fetch_all(&http, &mut lineup.picks).await as i64;
    report.timings.record("comments", elapsed_ms(stage));

    // --- Stage 9: editorial — summaries and the Brief (§14) ---
    let editorial = match curator.editorial_timed(&lineup).await {
        Ok((editorial, timings)) => {
            report.timings.record("summaries", timings.summaries_ms);
            report.timings.record("brief", timings.brief_ms);
            editorial
        }
        Err(e) => {
            report.warn(format!(
                "editorial generation failed; using excerpts: {e:#}"
            ));
            report.timings.record("summaries", 0);
            report.timings.record("brief", 0);
            editorial::fallback_editorial(&lineup)
        }
    };
    apply_summaries(&mut lineup, &editorial);

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
    let llm_cost = record_provider_costs(report, &meters);
    // Voyage rides along in `provider_costs_json` (§7.6) so `stats` can price
    // it per day; its tokens are embedding input, kept out of the LLM aggregate.
    report.provider_costs.insert(
        VOYAGE_PROVIDER.into(),
        ProviderUsage {
            usage: TokenUsage {
                input_tokens: report.voyage_tokens,
                ..TokenUsage::default()
            },
            cost_usd: report.voyage_cost_usd,
        },
    );
    let total_cost = llm_cost + report.voyage_cost_usd;
    let summary_model = match config.editorial.summary_model {
        crate::config::SummaryModel::Editor if curator.llms.editor.is_some() => {
            curator.llms.editor.as_ref().map(|c| c.model.clone())
        }
        _ => curator.llms.bulk.as_ref().map(|c| c.model.clone()),
    }
    .unwrap_or_else(|| "none".into());
    let provider_costs = report
        .provider_costs
        .iter()
        .map(|(provider, usage)| (provider.clone(), usage.cost_usd))
        .collect();
    let models = Models {
        bulk: curator
            .llms
            .bulk
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "none".into()),
        editor: match (&curator.llms.editor, &curator.llms.bulk) {
            (Some(editor), _) => editor.model.clone(),
            (None, Some(bulk)) => format!("{} (bulk fallback)", bulk.model),
            (None, None) => "none".into(),
        },
        summaries: summary_model,
    };
    let colophon = Colophon {
        provider_costs,
        models: models.clone(),
        entries_fetched: report.counts.entries_fetched,
        feeds_seen: report.counts.feeds_seen,
        candidates: report.counts.candidates,
        cost_usd: total_cost,
        generator_version: format!("daily-epub {}", crate::VERSION),
    };
    let behind = behind_the_paper(ctx, report, models, total_cost).await;
    let mut issue = build_issue(
        date,
        issue_number,
        Timestamp::now(),
        lineup,
        editorial,
        world_briefing,
        colophon,
    );
    issue.behind = behind;

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

/// Near misses listed in the "Behind the paper" chapter (§15.1).
const NEAR_MISSES_IN_PAPER: usize = 10;

/// The facts of §15.1, from the report so far and the run's `candidate_runs`
/// rows (selection telemetry must already be written). Never fails: a
/// telemetry read error leaves the near-miss list empty.
async fn behind_the_paper(
    ctx: &StageContext<'_>,
    report: &RunReport,
    models: Models,
    cost_usd: f64,
) -> BehindThePaper {
    let near_misses =
        match telemetry::paper_near_misses(ctx.db, ctx.run_id, NEAR_MISSES_IN_PAPER).await {
            Ok(misses) => misses,
            Err(error) => {
                tracing::warn!(%error, "could not read near misses for the paper");
                Vec::new()
            }
        };
    let counts = &report.counts;
    BehindThePaper {
        considered: counts.articles,
        feeds_seen: counts.feeds_seen,
        eligible: counts.eligible,
        triaged: counts.triaged,
        read_closely: counts.assessed,
        shortlisted: counts.shortlisted,
        selected: counts.selected,
        admitted_by: counts.admitted_by.clone(),
        rated_with_embeddings: counts.rated_with_embeddings,
        knn_gate: counts.knn_gate,
        feed_gate: counts.feed_gate,
        near_misses,
        models,
        embedding_model: if counts.embedded > 0 {
            ctx.config.voyage.model.clone()
        } else {
            "none".into()
        },
        cost_usd,
        generation_secs: (Timestamp::now().as_second() - ctx.started_at.as_second()).max(0),
    }
}

/// The embedding cache with a Voyage client behind it, or cache-only under
/// `--skip-embeddings`, `voyage.enabled = false` or a missing key (§16, §17).
fn build_embedding_service(
    ctx: &StageContext<'_>,
    report: &mut RunReport,
) -> embedding::EmbeddingService {
    let (db, voyage) = (ctx.db.clone(), ctx.config.voyage.clone());
    if ctx.skip_embeddings {
        tracing::info!("--skip-embeddings: using cached vectors only, no Voyage calls");
        return embedding::EmbeddingService::cached_only(db, voyage);
    }
    if !voyage.enabled {
        tracing::info!("voyage disabled: using cached embeddings only");
        return embedding::EmbeddingService::cached_only(db, voyage);
    }
    match embedding::EmbeddingService::real(db.clone(), voyage.clone()) {
        Ok(service) => service,
        Err(embedding::EmbeddingError::MissingApiKey) => {
            tracing::warn!(
                "voyage enabled but {} is unset; using cached embeddings only",
                embedding::VOYAGE_API_KEY_ENV
            );
            embedding::EmbeddingService::cached_only(db, voyage)
        }
        Err(error) => {
            report.warn(format!(
                "Voyage unavailable; using cached embeddings only: {error}"
            ));
            embedding::EmbeddingService::cached_only(db, voyage)
        }
    }
}

/// Embeddings and cheap signals for every hygiene-eligible candidate (§9).
async fn prepare_features(
    ctx: &StageContext<'_>,
    candidates: &mut [Candidate],
    service: &embedding::EmbeddingService,
    report: &mut RunReport,
) -> HashMap<ArticleId, Vec<f32>> {
    let (config, db) = (ctx.config, ctx.db);
    let eligible = candidates
        .iter()
        .map(|candidate| candidate.article.clone())
        .collect::<Vec<_>>();

    // --- embed (§7.1, §7.2) ---
    let stage = Timestamp::now();
    let article_embeddings = match service.articles(&eligible).await {
        Ok(embeddings) => embeddings,
        Err(error) => {
            report.warn(format!("article embedding stage degraded: {error}"));
            HashMap::new()
        }
    };
    report.counts.embedded = article_embeddings.len() as i64;
    let interests =
        match profile::load_standing_interests(&config.interests_opml, &config.profile_path) {
            Ok(interests) => interests,
            Err(error) => {
                tracing::warn!(%error, "could not load standing interests for embeddings");
                Vec::new()
            }
        };
    let interest_embeddings = match service.interests(&interests).await {
        Ok(embeddings) => embeddings,
        Err(error) => {
            report.warn(format!("interest embedding stage degraded: {error}"));
            HashMap::new()
        }
    };
    if let Some(meter) = service.meter() {
        report.voyage_tokens = meter.total_tokens();
        report.voyage_cost_usd = meter.cost_usd();
    }
    tracing::info!(
        eligible = eligible.len(),
        embedded = article_embeddings.len(),
        interests = interest_embeddings.len(),
        voyage_tokens = report.voyage_tokens,
        "embeddings ready"
    );
    report.timings.record("embed", elapsed_ms(stage));

    // --- signals (§9, §12.2, §12.4) ---
    let stage = Timestamp::now();
    let ranking = &config.curation.ranking;
    let (mut computed, preference) = match signals::compute_all(
        db,
        &eligible,
        &article_embeddings,
        &interest_embeddings,
        &config.voyage,
        ranking,
        Timestamp::now(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            report.warn(format!("signal computation degraded: {error:#}"));
            let state = signals::PreferenceState::default();
            (
                signals::compute(
                    &eligible,
                    &article_embeddings,
                    &interest_embeddings,
                    &state,
                    ranking,
                ),
                state.summary(),
            )
        }
    };
    report.counts.rated_with_embeddings = preference.rated_with_embeddings as i64;
    report.counts.knn_gate = preference.knn_gate;
    report.counts.feed_gate = preference.feed_gate;
    for candidate in candidates.iter_mut() {
        candidate.signals = computed
            .remove(&candidate.article.id)
            .unwrap_or_else(|| signals::Signals::baseline(&candidate.article));
    }
    if let Err(error) = record_candidates(ctx, candidates).await {
        report.warn(format!("could not record eligible candidates: {error}"));
    }
    report.timings.record("signals", elapsed_ms(stage));
    article_embeddings
}

async fn record_candidates(ctx: &StageContext<'_>, candidates: &[Candidate]) -> Result<()> {
    record_candidates_with_why(ctx, candidates, &HashMap::new()).await
}

async fn record_candidates_with_why(
    ctx: &StageContext<'_>,
    candidates: &[Candidate],
    editor_why: &HashMap<ArticleId, Option<&str>>,
) -> Result<()> {
    for candidate in candidates {
        let json = telemetry::serialize_candidate(candidate);
        let admitted_by = if candidate.admitted_by.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&candidate.admitted_by)?)
        };
        telemetry::write(
            ctx.db,
            &telemetry::CandidateRun {
                run_id: ctx.run_id,
                article_id: candidate.article.id,
                stage: &candidate.stage,
                excluded_reason: candidate.excluded_reason.as_deref(),
                admitted_by: admitted_by.as_deref(),
                signals_json: &json,
                utility: candidate.utility,
                rank_utility: candidate.rank_utility,
                cluster_id: candidate.cluster,
                cluster_rank: candidate.cluster_rank,
                editor_why: editor_why.get(&candidate.article.id).copied().flatten(),
            },
        )
        .await
        .with_context(|| {
            format!(
                "recording candidate {} at stage {}",
                candidate.article.id, candidate.stage
            )
        })?;
    }
    Ok(())
}

fn set_candidate_stage(
    candidates: &mut [Candidate],
    ids: &[ArticleId],
    stage: &str,
    excluded_reason: Option<&str>,
) {
    let ids = ids.iter().copied().collect::<HashSet<_>>();
    for candidate in candidates {
        if ids.contains(&candidate.article.id) {
            candidate.stage = stage.into();
            candidate.excluded_reason = excluded_reason.map(str::to_string);
        }
    }
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

    let mut stored_issue = issue.clone();
    for pick in &mut stored_issue.lineup.picks {
        pick.article.content_html.clear();
    }
    let issue_json = serde_json::to_string(&stored_issue).context("serializing issue snapshot")?;

    db.upsert_issue(
        issue.meta.date,
        issue.meta.issue_number,
        issue.meta.generated_at,
        epub_path.as_deref(),
        x4_path.as_deref(),
        xtc_path.as_deref(),
        Some(&issue.editorial.front_page_html),
        None,
        Some(&issue_json),
    )
    .await?;
    db.replace_issue_articles(issue.meta.date, &issue.lineup.picks)
        .await?;
    Ok(())
}

/// Build the bulk and editor clients named in `[llm]`, running the weekly
/// profile rebuild when it is due.
///
/// Each client is `None` for `--skip-llm` and for every configuration/API
/// problem: the pipeline then degrades per §17 instead of failing the run.
async fn build_llms(
    ctx: &StageContext<'_>,
    meters: &BTreeMap<String, UsageMeter>,
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
    report.counts.verdicts_in_prompt = profile.verdicts as i64;
    if ctx.skip_llm {
        tracing::info!("--skip-llm: profile rebuilt; no provider calls will be made");
        return Llms::default();
    }

    let make_clients = |prompt: String| Llms::from_config(ctx.config, prompt, meters);

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
            report.counts.verdicts_in_prompt = rebuilt.verdicts as i64;
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

/// Startup lines naming each role's resolved provider and whether it is on
/// (§19): the root config ignores unknown sections, so a `[voyages]` typo
/// would otherwise be silent. Keys are never logged, only their presence.
fn log_resolved_providers(config: &Config, skip_llm: bool, skip_embeddings: bool) {
    let has_key = |key: Option<&str>| key.is_some_and(|k| !k.trim().is_empty());
    for (role, name) in config.llm.roles() {
        match config.providers.get(name) {
            Some(provider) => tracing::info!(
                role,
                provider = name,
                kind = provider.kind.as_str(),
                model = %provider.model,
                effort = provider.effort.as_deref().unwrap_or("-"),
                enabled = !skip_llm && provider.api_key().is_some(),
                key_present = provider.api_key().is_some(),
                max_daily_usd = provider.max_daily_usd,
                "resolved llm role"
            ),
            None => tracing::error!(role, provider = name, "role names an unknown provider"),
        }
    }
    if config.llm.bulk_name().is_none() {
        tracing::info!("no bulk provider: triage and deep assessment are skipped");
    }
    if config.llm.editor_name().is_none() {
        tracing::info!("no editor provider: editor work runs on bulk");
    }
    tracing::info!(
        summary_model = ?config.editorial.summary_model,
        embedding_model = %config.voyage.model,
        embedding_enabled = !skip_embeddings
            && config.voyage.enabled
            && has_key(config.voyage.api_key.as_deref()),
        embedding_dimension = config.voyage.output_dimension,
        embedding_max_daily_usd = config.voyage.max_daily_usd,
        "resolved providers"
    );
}

/// Every referenced provider's usage into `report.provider_costs`, keyed by
/// its `[providers.*]` name; returns the summed LLM cost.
fn record_provider_costs(report: &mut RunReport, meters: &BTreeMap<String, UsageMeter>) -> f64 {
    let mut total = 0.0;
    for (name, meter) in meters {
        let cost_usd = meter.cost_usd();
        total += cost_usd;
        report.provider_costs.insert(
            name.clone(),
            ProviderUsage {
                usage: meter.total(),
                cost_usd,
            },
        );
    }
    total
}

/// Prompt versions recorded per run so old telemetry stays interpretable (§7.6).
/// Bump a number when the corresponding instruction block changes.
const PROMPT_VERSIONS: &[(&str, u32)] = &[
    ("triage", triage::TRIAGE_PROMPT_VERSION as u32),
    ("deep", crate::curate::assess::DEEP_PROMPT_VERSION as u32),
    ("editor", 2),
    ("summary", 1),
    ("brief", 2),
    ("profile", 2),
];

/// The resolved `[curation]` (ranking included), `[editorial]`, `[voyage]`,
/// `[llm]`, the provider registry, model names and prompt versions written to
/// `runs.config_json` (§7.6, §19). Never includes keys.
fn resolved_run_config(config: &Config, soft_target: usize, hard_max: usize) -> serde_json::Value {
    let mut curation = config.curation.clone();
    curation.max_article_count = hard_max;
    let mut voyage = config.voyage.clone();
    voyage.api_key = None;
    let model_of = |role: Option<(&str, &crate::config::ProviderConfig)>| {
        role.map(|(_, provider)| provider.model.clone())
            .unwrap_or_else(|| "disabled".into())
    };
    serde_json::json!({
        "target_article_count": soft_target,
        "TRIAGE_PROMPT_VERSION": triage::TRIAGE_PROMPT_VERSION,
        "DEEP_PROMPT_VERSION": crate::curate::assess::DEEP_PROMPT_VERSION,
        "curation": curation,
        "editorial": config.editorial,
        "voyage": voyage,
        "llm": config.llm,
        "providers": config.providers_redacted(),
        "models": {
            "bulk": model_of(config.bulk_provider()),
            "editor": model_of(config.editor_provider()),
            "editor_effort": config
                .editor_provider()
                .and_then(|(_, provider)| provider.effort.clone()),
            "embedding": if config.voyage.enabled { config.voyage.model.as_str() } else { "disabled" },
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
        for provider in config.providers.values_mut() {
            provider.api_key = Some("sk-secret".into());
        }
        config.voyage.api_key = Some("pa-secret".into());
        let value = resolved_run_config(&config, 6, 6);
        assert_eq!(value["target_article_count"], 6);
        assert_eq!(value["curation"]["max_article_count"], 6);
        assert_eq!(value["curation"]["ranking"]["deep_keep"], 120);
        assert_eq!(
            value["curation"]["ranking"]["weights"]["utility"]["quality"],
            0.40
        );
        assert_eq!(value["voyage"]["model"], "voyage-4-lite");
        assert_eq!(value["voyage"]["output_dimension"], 512);
        assert!(value["voyage"]["api_key"].is_null());
        assert_eq!(value["models"]["embedding"], "voyage-4-lite");
        assert_eq!(value["editorial"]["summary_model"], "editor");
        assert_eq!(value["editorial"]["summary_input_tokens"], 3000);
        assert_eq!(value["models"]["bulk"], "deepseek-v4-flash");
        assert_eq!(value["models"]["editor"], "claude-opus-5");
        assert_eq!(value["models"]["editor_effort"], "high");
        assert_eq!(value["llm"]["bulk"], "deepseek");
        assert_eq!(value["llm"]["editor"], "anthropic");
        assert_eq!(value["llm"]["triage_batch_size"], 25);
        assert_eq!(value["providers"]["gemini"]["kind"], "openai");
        assert_eq!(value["providers"]["anthropic"]["max_daily_usd"], 3.0);
        for provider in value["providers"].as_object().expect("providers") {
            assert!(
                provider.1["api_key"].is_null(),
                "{} leaked its key",
                provider.0
            );
        }
        assert!(value["prompt_versions"]["editor"].is_number());
        assert_eq!(
            value["TRIAGE_PROMPT_VERSION"],
            triage::TRIAGE_PROMPT_VERSION
        );
        assert_eq!(
            value["DEEP_PROMPT_VERSION"],
            crate::curate::assess::DEEP_PROMPT_VERSION
        );
        assert_eq!(
            value["prompt_versions"]["triage"],
            triage::TRIAGE_PROMPT_VERSION
        );
        let text = value.to_string();
        assert!(
            !text.contains("secret"),
            "keys must never reach the database"
        );

        let mut config = Config::default();
        config.llm.editor.clear();
        let value = resolved_run_config(&config, 6, 6);
        assert_eq!(value["models"]["editor"], "disabled");
        assert!(value["models"]["editor_effort"].is_null());
    }

    /// `provider_costs` is keyed by whatever the operator named the providers,
    /// never by a hard-coded "deepseek" / "anthropic".
    #[test]
    fn provider_costs_are_keyed_by_the_configured_provider_names() {
        let mut config = Config::default();
        let bulk = config.providers.remove("deepseek").expect("deepseek");
        config.providers.insert("bulkprov".into(), bulk);
        config.llm.bulk = "bulkprov".into();
        config.llm.editor = "gemini".into();
        config.validate().expect("renamed provider validates");

        let meters = provider_meters(&config);
        assert_eq!(
            meters.keys().collect::<Vec<_>>(),
            vec!["bulkprov", "gemini"]
        );
        meters["bulkprov"].record(TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        });
        let mut report = RunReport::new(run_date(), now());
        let total = record_provider_costs(&mut report, &meters);
        assert_eq!(
            report.provider_costs.keys().collect::<Vec<_>>(),
            vec!["bulkprov", "gemini"]
        );
        assert!(!report.provider_costs.contains_key("deepseek"));
        assert!((report.provider_costs["bulkprov"].cost_usd - 0.14).abs() < 1e-9);
        assert_eq!(report.provider_costs["gemini"].cost_usd, 0.0);
        assert!((total - 0.14).abs() < 1e-9);
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

    use std::sync::Arc;

    use crate::curate::embedding::{EmbeddingClient, EmbeddingService, MockBackend};
    use crate::curate::llm::{LlmClient, MockBackend as ChatMockBackend};
    use crate::types::{Entry, ExtractMethod, SourceKind, SourceRef};
    use sqlx::Row as _;

    fn now() -> Timestamp {
        "2026-09-02T09:00:00Z".parse().unwrap()
    }

    fn run_date() -> Date {
        "2026-09-02".parse().unwrap()
    }

    fn fixture_article(entry_id: i64, host: &str, words: usize) -> Article {
        let url = format!("https://{host}/post-{entry_id}");
        let body = (0..words)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        Article {
            id: 0,
            canonical_url: url.clone(),
            title: format!("Post {entry_id}"),
            best_entry_id: entry_id,
            content_html: format!("<p>{body}</p>"),
            word_count: words as i64,
            excerpt_only: false,
            image_count: 0,
            sources: vec![SourceRef {
                entry_id,
                feed_id: 100 + entry_id,
                feed_title: format!("Feed {entry_id}"),
                category: None,
                kind: SourceKind::Feed,
            }],
            first_seen: now(),
            url,
            author: None,
            feed_id: 100 + entry_id,
            feed_title: format!("Feed {entry_id}"),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        }
    }

    fn entry_for(article: &Article) -> Entry {
        Entry {
            id: article.best_entry_id,
            feed_id: article.feed_id,
            feed_title: Some(article.feed_title.clone()),
            category: None,
            title: article.title.clone(),
            url: article.url.clone(),
            canonical_url: Some(article.canonical_url.clone()),
            author: None,
            published_at: None,
            comments_url: None,
            raw_content: article.content_html.clone(),
            fetched_at: now(),
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        db: Db,
        config: Config,
        articles: Vec<Article>,
        run_id: i64,
    }

    /// Four articles: two ordinary, one on a blocked host, one published yesterday.
    async fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("run.db"))
            .await
            .unwrap();
        let mut config = Config::default();
        config.curation.blocked_domains = vec!["blocked.example".into()];
        config.voyage.output_dimension = 4;
        config.target_article_count = 1;
        config.interests_opml = dir.path().join("interests.opml");
        std::fs::write(
            &config.interests_opml,
            "<opml><body><outline text=\"Writerdeck\"/></body></opml>",
        )
        .unwrap();
        config.profile_path = dir.path().join("profile.md");
        std::fs::write(&config.profile_path, "# Reader profile\n").unwrap();

        let mut articles = vec![
            fixture_article(1, "a.example", 1200),
            fixture_article(2, "b.example", 900),
            fixture_article(3, "blocked.example", 1500),
            fixture_article(4, "d.example", 1400),
        ];
        let entries = articles.iter().map(entry_for).collect::<Vec<_>>();
        db.upsert_entries(&entries).await.unwrap();
        persist_articles(&db, &mut articles).await.unwrap();
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at)
             VALUES ('2026-09-01', 1, '2026-09-01T12:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issue_articles (issue_date, article_id, section)
             VALUES ('2026-09-01', ?, 'Top Stories')",
        )
        .bind(articles[3].id)
        .execute(db.pool())
        .await
        .unwrap();
        let run_id = db.start_run(run_date(), now()).await.unwrap();
        Harness {
            _dir: dir,
            db,
            config,
            articles,
            run_id,
        }
    }

    fn context<'a>(h: &'a Harness, skip_embeddings: bool) -> StageContext<'a> {
        StageContext {
            config: &h.config,
            db: &h.db,
            run_id: h.run_id,
            date: run_date(),
            soft_target: h.config.target_article_count,
            hard_max: h.config.curation.max_article_count,
            started_at: now(),
            out_dir: PathBuf::from("."),
            dry_run: true,
            skip_llm: true,
            skip_embeddings,
            rescore: false,
        }
    }

    fn mock_service(h: &Harness, backend: Arc<MockBackend>) -> EmbeddingService {
        let client = EmbeddingClient::with_backend(h.config.voyage.clone(), backend);
        EmbeddingService::with_client(h.db.clone(), h.config.voyage.clone(), client)
    }

    async fn stage_rows(
        db: &Db,
        run_id: i64,
    ) -> BTreeMap<i64, (String, Option<String>, Option<String>)> {
        sqlx::query(
            "SELECT article_id, stage, excluded_reason, admitted_by FROM candidate_runs
             WHERE run_id = ? ORDER BY article_id",
        )
        .bind(run_id)
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get::<i64, _>("article_id"),
                (
                    row.get::<String, _>("stage"),
                    row.get::<Option<String>, _>("excluded_reason"),
                    row.get::<Option<String>, _>("admitted_by"),
                ),
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn mocked_run_writes_a_candidate_runs_row_for_every_considered_article() {
        let h = harness().await;
        let ctx = context(&h, false);
        let backend = Arc::new(MockBackend::auto(4));
        let service = mock_service(&h, backend.clone());
        let mut report = RunReport::new(run_date(), now());

        let mut features = admit::hygiene(
            &h.db,
            h.run_id,
            h.articles.clone(),
            run_date(),
            &h.config.curation,
            now(),
        )
        .await
        .unwrap();
        report.counts.eligible = features.len() as i64;
        prepare_features(&ctx, &mut features, &service, &mut report).await;
        let [a, b, blocked, published] = [
            h.articles[0].id,
            h.articles[1].id,
            h.articles[2].id,
            h.articles[3].id,
        ];
        assert_eq!(
            features
                .iter()
                .map(|candidate| candidate.article.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([a, b])
        );
        assert_eq!(report.counts.eligible, 2);
        assert_eq!(report.counts.embedded, 2);
        assert_eq!(report.counts.rated_with_embeddings, 0);
        assert!(report.timings.0.contains_key("embed") && report.timings.0.contains_key("signals"));
        assert!(report.voyage_tokens > 0);
        // One batch for the two articles, one for the interest.
        assert_eq!(backend.calls(), 2);
        let signals = &features
            .iter()
            .find(|candidate| candidate.article.id == a)
            .unwrap()
            .signals;
        assert!(signals.heuristic.is_some());
        assert!(
            signals.interest.is_some(),
            "interest present under the raw fallback"
        );
        assert!(
            signals.knn.is_none() && signals.feed.is_none(),
            "gates closed"
        );
        assert!(signals.preliminary.is_some());

        let rows = stage_rows(&h.db, h.run_id).await;
        assert_eq!(rows.len(), 4, "one row per considered article");
        assert_eq!(rows[&blocked].0, "excluded");
        assert_eq!(rows[&blocked].1.as_deref(), Some("blocked"));
        assert_eq!(rows[&published].0, "excluded");
        assert_eq!(rows[&published].1.as_deref(), Some("published_before"));
        assert_eq!(rows[&a].0, "eligible");
        assert_eq!(rows[&a].1, None);
        let thin: String =
            sqlx::query_scalar("SELECT signals_json FROM candidate_runs WHERE article_id = ?")
                .bind(blocked)
                .fetch_one(h.db.pool())
                .await
                .unwrap();
        assert_eq!(thin, "{}");

        // Admission replaces the old prefilter and carries retriever telemetry.
        // The bulk provider is "down": the client exists but the one deep batch
        // fails transiently through every retry (a content-filter rejection
        // would be bisected instead), so the deep set is ranked on present
        // signals and the editor falls back to utility order (§17).
        let bulk_backend = Arc::new(ChatMockBackend::new());
        for _ in 0..3 {
            bulk_backend.push_llm_error(crate::curate::llm::LlmError::Transient {
                provider: "deepseek".into(),
                message: "503".into(),
            });
        }
        let bulk = LlmClient::with_backend(
            &h.config.providers["deepseek"].model,
            "SYSTEM".into(),
            UsageMeter::for_provider(&h.config.providers["deepseek"]),
            bulk_backend.clone(),
        )
        .with_retry(crate::http::RetryPolicy {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        });
        let curator = Curator::new(
            h.config.clone(),
            h.db.clone(),
            Llms {
                bulk: Some(bulk),
                editor: None,
            },
        );
        admit::admit(&mut features, run_date(), &h.config.curation.ranking);
        record_candidates(&ctx, &features).await.unwrap();
        let admitted = features
            .iter()
            .filter(|candidate| candidate.stage == "admitted")
            .map(|candidate| candidate.article.id)
            .collect::<Vec<_>>();
        assert_eq!(
            admitted.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([a, b])
        );

        let assessed = curator
            .assess(&mut features, false, None, now())
            .await
            .unwrap()
            .assessed();
        assert_eq!(assessed, 0, "every deep batch failed");
        assert_eq!(
            bulk_backend.calls(),
            3,
            "one batch was attempted, three times; nothing was bisected"
        );
        let rejected: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM article_assessments WHERE kind = 'provider_rejected'",
        )
        .fetch_one(h.db.pool())
        .await
        .unwrap();
        assert_eq!(rejected, 0, "a transient failure is not a rejection");
        assert!(features.iter().all(|c| c.assessment.deep.is_none()));
        let embeddings = features
            .iter()
            .map(|candidate| (candidate.article.id, vec![1.0, 0.0, 0.0, 0.0]))
            .collect::<HashMap<_, _>>();
        let ranked = rank::shortlist(&mut features, &embeddings, &h.config.curation.ranking);
        assert_eq!(ranked.shortlisted, 2);
        assert_eq!(ranked.clusters, 1, "identical embeddings share a leader");
        record_candidates(&ctx, &features).await.unwrap();
        let rows = sqlx::query(
            "SELECT article_id, stage, utility, rank_utility, cluster_id, cluster_rank
                 FROM candidate_runs WHERE run_id = ? AND stage = 'shortlisted'
                 ORDER BY rank_utility",
        )
        .bind(h.run_id)
        .fetch_all(h.db.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "both admitted articles were shortlisted");
        for (index, row) in rows.iter().enumerate() {
            let rank = index as i64 + 1;
            assert_eq!(row.get::<String, _>("stage"), "shortlisted");
            assert!(
                row.get::<Option<f64>, _>("utility").is_some(),
                "utility over present signals"
            );
            assert_eq!(row.get::<Option<i64>, _>("rank_utility"), Some(rank));
            assert_eq!(row.get::<Option<i64>, _>("cluster_id"), Some(1));
            assert_eq!(row.get::<Option<i64>, _>("cluster_rank"), Some(rank));
        }
        let best = rows[0].get::<i64, _>("article_id");

        let mut candidates = features
            .iter()
            .filter(|candidate| candidate.stage == "shortlisted")
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| candidate.rank_utility.unwrap_or(i64::MAX));
        let lineup = curator.select(candidates, run_date()).await.unwrap();
        assert_eq!(
            bulk_backend.calls(),
            4,
            "the editor tried the bulk fallback"
        );
        let selected = lineup
            .picks
            .iter()
            .map(|p| p.article.id)
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0], best,
            "without any LLM the lineup follows utility"
        );
        let not_selected = admitted
            .iter()
            .copied()
            .filter(|id| !selected.contains(id))
            .collect::<Vec<_>>();
        set_candidate_stage(&mut features, &selected, "selected", None);
        set_candidate_stage(
            &mut features,
            &not_selected,
            "shortlisted",
            Some("not_selected"),
        );
        record_candidates(&ctx, &features).await.unwrap();

        let rows = stage_rows(&h.db, h.run_id).await;
        assert_eq!(rows.len(), 4);
        let (winner, loser) = (selected[0], not_selected[0]);
        assert_eq!(
            rows[&winner],
            (
                "selected".into(),
                None,
                Some("[\"interest\",\"blend\"]".into())
            )
        );
        assert_eq!(
            rows[&loser],
            (
                "shortlisted".into(),
                Some("not_selected".into()),
                Some("[\"interest\",\"blend\"]".into())
            )
        );
        let text = telemetry::explain(
            &h.db,
            run_date(),
            Some(h.run_id),
            &telemetry::ExplainTarget::Article(loser),
        )
        .await
        .unwrap();
        assert!(
            text.contains("stage: shortlisted · reason: not_selected"),
            "{text}"
        );
        assert!(
            text.contains("utility: ") && text.contains(" · rank 2"),
            "{text}"
        );
        assert!(text.contains("cluster: 1 · rank 2"), "{text}");
        assert!(text.contains("quality      absent"), "{text}");
        let misses = telemetry::explain_near_misses(&h.db, run_date(), Some(h.run_id), 5)
            .await
            .unwrap();
        assert!(misses.contains("not selected, by utility"), "{misses}");
        assert!(misses.contains("shortlisted, not_selected"), "{misses}");

        // The Behind-the-paper facts come from the same rows and the report.
        report.counts.articles = 4;
        report.counts.feeds_seen = 4;
        report.counts.admitted = 2;
        report.counts.admitted_by = BTreeMap::from([("interest".to_string(), 2)]);
        report.counts.shortlisted = 2;
        report.counts.selected = 1;
        let behind = behind_the_paper(&ctx, &report, Models::default(), 0.25).await;
        assert_eq!(behind.considered, 4);
        assert_eq!(behind.feeds_seen, 4);
        assert_eq!(behind.eligible, 2);
        assert_eq!(behind.shortlisted, 2);
        assert_eq!(behind.selected, 1);
        assert_eq!(behind.admitted_by.get("interest"), Some(&2));
        assert_eq!(behind.rated_with_embeddings, 0);
        assert_eq!(behind.knn_gate, 0.0);
        assert_eq!(behind.embedding_model, h.config.voyage.model);
        assert_eq!(behind.cost_usd, 0.25);
        assert_eq!(behind.near_misses.len(), 1, "one shortlisted, not selected");
        let miss = &behind.near_misses[0];
        assert_eq!(miss.article_id, loser);
        assert_eq!(
            miss.title,
            format!(
                "Post {}",
                h.articles
                    .iter()
                    .find(|a| a.id == loser)
                    .unwrap()
                    .best_entry_id
            )
        );
        assert_eq!(
            miss.feed_title,
            h.articles
                .iter()
                .find(|a| a.id == loser)
                .unwrap()
                .feed_title
        );
        assert_eq!(miss.stage, "shortlisted");
        assert_eq!(miss.reason.as_deref(), Some("not_selected"));
        assert!(miss.quality.is_none(), "no deep assessment ran");
        let chapter = crate::epub::chapters::render_behind_the_paper(&Issue {
            behind: behind.clone(),
            ..crate::epub::fixtures::issue()
        })
        .unwrap();
        assert!(
            chapter.xhtml.contains("Considered 4 articles from 4 feeds"),
            "{}",
            chapter.xhtml
        );
        assert!(chapter.xhtml.contains(&miss.title), "{}", chapter.xhtml);
        assert!(
            chapter.xhtml.contains("shortlisted, not selected"),
            "{}",
            chapter.xhtml
        );
    }

    #[tokio::test]
    async fn skip_embeddings_makes_zero_voyage_calls_and_uses_the_cache() {
        let h = harness().await;
        let ctx = context(&h, true);
        let mut report = RunReport::new(run_date(), now());
        let service = build_embedding_service(&ctx, &mut report);
        assert!(!service.has_client(), "--skip-embeddings is cache-only");
        assert!(service.meter().is_none());
        let mut features = admit::hygiene(
            &h.db,
            h.run_id,
            h.articles.clone(),
            run_date(),
            &h.config.curation,
            now(),
        )
        .await
        .unwrap();
        report.counts.eligible = features.len() as i64;
        prepare_features(&ctx, &mut features, &service, &mut report).await;
        assert_eq!(features.len(), 2);
        assert_eq!(report.counts.embedded, 0, "nothing cached yet");
        assert!(features.iter().all(|f| f.signals.interest.is_none()));
        assert!(features.iter().all(|f| f.signals.heuristic.is_some()));
        assert_eq!(report.voyage_tokens, 0);
    }

    #[tokio::test]
    async fn a_voyage_failure_degrades_to_absent_signals_and_the_run_continues() {
        let h = harness().await;
        let ctx = context(&h, false);
        let backend = Arc::new(MockBackend::new()); // nothing scripted: every call fails
        let service = mock_service(&h, backend.clone());
        let mut report = RunReport::new(run_date(), now());
        let mut features = admit::hygiene(
            &h.db,
            h.run_id,
            h.articles.clone(),
            run_date(),
            &h.config.curation,
            now(),
        )
        .await
        .unwrap();
        report.counts.eligible = features.len() as i64;
        prepare_features(&ctx, &mut features, &service, &mut report).await;
        assert!(backend.calls() >= 1);
        assert_eq!(features.len(), 2);
        assert_eq!(report.counts.eligible, 2);
        assert_eq!(report.counts.embedded, 0);
        assert!(report.error.is_none());
        for feature in &features {
            assert!(feature.signals.interest.is_none() && feature.signals.knn.is_none());
            assert!(feature.signals.heuristic.is_some());
            assert!(
                feature.signals.preliminary.is_some(),
                "scored on what is present"
            );
        }
        assert_eq!(stage_rows(&h.db, h.run_id).await.len(), 4);
    }
}
