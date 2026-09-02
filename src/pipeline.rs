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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::civil::Date;
use jiff::{Timestamp, Zoned};

use crate::config::Config;
use crate::curate::llm::{Llms, PriceTable, UsageMeter};
use crate::curate::{Curator, editorial, embedding, prefilter, profile, signals, telemetry};
use crate::db::Db;
use crate::extract::Extractor;
use crate::miniflux::MinifluxClient;
use crate::publish::Published;
use crate::report::{ProviderUsage, RunReport, RunStatus};
use crate::types::{
    Article, ArticleId, Artifact, Colophon, Edition, Issue, IssueMeta, Lineup, Models,
    reading_minutes,
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
    /// `--skip-embeddings`: read the cache but make zero Voyage calls.
    pub skip_embeddings: bool,
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
        skip_embeddings = opts.skip_embeddings,
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
    run_id: i64,
    date: Date,
    soft_target: usize,
    hard_max: usize,
    started_at: Timestamp,
    out_dir: PathBuf,
    dry_run: bool,
    skip_llm: bool,
    skip_embeddings: bool,
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

    // --- Stage 6: hygiene, embeddings, and cheap signals (§8.1, §9) ---
    let embeddings = build_embedding_service(ctx, report);
    let feature_signals = prepare_features(ctx, &articles, &embeddings, report).await;

    // --- Stage 6b: the old heuristic pre-filter still gates in this step (§21) ---
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
    let admitted = candidates
        .iter()
        .map(|candidate| candidate.article.id)
        .collect::<Vec<_>>();
    let admitted_set = admitted.iter().copied().collect::<HashSet<_>>();
    let not_admitted = feature_signals
        .keys()
        .copied()
        .filter(|id| !admitted_set.contains(id))
        .collect::<Vec<_>>();
    record_stage(
        ctx,
        &feature_signals,
        &not_admitted,
        "eligible",
        Some("not_admitted"),
    )
    .await
    .context("recording prefilter telemetry")?;
    record_stage(ctx, &feature_signals, &admitted, "admitted", None)
        .await
        .context("recording prefilter telemetry")?;
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
    let assessed = candidates
        .iter()
        .filter(|candidate| candidate.llm.is_some())
        .map(|candidate| candidate.article.id)
        .collect::<Vec<_>>();
    record_stage(ctx, &feature_signals, &assessed, "assessed", None)
        .await
        .context("recording assessment telemetry")?;
    // Every prefilter survivor goes to the old selector, scored or not.
    record_stage(ctx, &feature_signals, &admitted, "shortlisted", None)
        .await
        .context("recording shortlist telemetry")?;

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
    let not_selected = admitted
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
    record_stage_with_why(ctx, &feature_signals, &selected_with_why, "selected", None)
        .await
        .context("recording selection telemetry")?;
    record_stage(
        ctx,
        &feature_signals,
        &not_selected,
        "shortlisted",
        Some("not_selected"),
    )
    .await
    .context("recording selection telemetry")?;
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

/// The cheap signals and hygiene outcome for one eligible article (§9).
#[derive(Debug, Clone)]
struct FeatureSignals {
    signals: signals::Signals,
    auto_include: bool,
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

/// Hygiene, embeddings and cheap signals for every article (§8.1, §9).
///
/// Hygiene-excluded articles get thin `candidate_runs` rows; every other
/// article gets an `eligible` row with its `signals_json`. Nothing here can
/// fail the run: embeddings and the learned signals degrade to absent (§17).
async fn prepare_features(
    ctx: &StageContext<'_>,
    articles: &[Article],
    service: &embedding::EmbeddingService,
    report: &mut RunReport,
) -> HashMap<ArticleId, FeatureSignals> {
    let (config, db) = (ctx.config, ctx.db);
    let hygiene = match prefilter::PrefilterContext::load(db, ctx.date).await {
        Ok(context) => context,
        Err(error) => {
            report.warn(format!(
                "could not load hygiene history; signals skipped: {error}"
            ));
            return HashMap::new();
        }
    };
    let published = hygiene
        .already_published
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let rejected = hygiene
        .recently_rejected
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut eligible = Vec::new();
    for article in articles {
        let auto_include = prefilter::is_auto_include(article, &config.curation);
        let reason = if published.contains(&article.id) {
            Some("published_before")
        } else if !auto_include && prefilter::is_blocked(article, &config.curation) {
            Some("blocked")
        } else if !auto_include && rejected.contains(&article.id) {
            Some("recently_rejected")
        } else {
            None
        };
        match reason {
            Some(reason) => {
                if let Err(error) =
                    telemetry::thin_excluded(db, ctx.run_id, article.id, reason).await
                {
                    report.warn(format!(
                        "could not record excluded candidate {}: {error}",
                        article.id
                    ));
                }
            }
            None => eligible.push(article.clone()),
        }
    }
    report.counts.eligible = eligible.len() as i64;

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
    let mut output = HashMap::new();
    for article in &eligible {
        let auto_include = prefilter::is_auto_include(article, &config.curation);
        let signals = computed
            .remove(&article.id)
            .unwrap_or_else(|| signals::Signals::baseline(article));
        output.insert(
            article.id,
            FeatureSignals {
                signals,
                auto_include,
            },
        );
    }
    let eligible_ids = eligible
        .iter()
        .map(|article| article.id)
        .collect::<Vec<_>>();
    if let Err(error) = record_stage(ctx, &output, &eligible_ids, "eligible", None).await {
        report.warn(format!("could not record eligible candidates: {error}"));
    }
    report.timings.record("signals", elapsed_ms(stage));
    output
}

/// Upsert the `candidate_runs` row of every listed article at a new stage
/// (§7.4). Articles without signals (hygiene-excluded) are left alone.
async fn record_stage(
    ctx: &StageContext<'_>,
    features: &HashMap<ArticleId, FeatureSignals>,
    ids: &[ArticleId],
    stage: &str,
    excluded_reason: Option<&str>,
) -> Result<()> {
    let rows = ids.iter().map(|id| (*id, None)).collect::<Vec<_>>();
    record_stage_with_why(ctx, features, &rows, stage, excluded_reason).await
}

/// [`record_stage`] with the editor's `why` per article (§13, §7.4).
async fn record_stage_with_why(
    ctx: &StageContext<'_>,
    features: &HashMap<ArticleId, FeatureSignals>,
    rows: &[(ArticleId, Option<&str>)],
    stage: &str,
    excluded_reason: Option<&str>,
) -> Result<()> {
    let admitted = matches!(stage, "admitted" | "assessed" | "shortlisted" | "selected");
    for (id, editor_why) in rows {
        let Some(feature) = features.get(id) else {
            continue;
        };
        let json = telemetry::serialize_signals(&feature.signals, feature.auto_include);
        let admitted_by = admitted.then_some(if feature.auto_include {
            "[\"auto\"]"
        } else {
            "[\"prefilter\"]"
        });
        telemetry::write(
            ctx.db,
            &telemetry::CandidateRun {
                run_id: ctx.run_id,
                article_id: *id,
                stage,
                excluded_reason,
                admitted_by,
                signals_json: &json,
                utility: None,
                rank_utility: None,
                cluster_id: None,
                cluster_rank: None,
                editor_why: *editor_why,
            },
        )
        .await
        .with_context(|| format!("recording candidate {id} at stage {stage}"))?;
    }
    Ok(())
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
/// (§19): the root config ignores unknown sections, so an `[anthropics]` or
/// `[voyages]` typo would otherwise be silent. Keys are never logged, only
/// their presence.
fn log_resolved_providers(config: &Config, skip_llm: bool, skip_embeddings: bool) {
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
        embedding_model = %config.voyage.model,
        embedding_enabled = !skip_embeddings
            && config.voyage.enabled
            && has_key(config.voyage.api_key.as_deref()),
        embedding_dimension = config.voyage.output_dimension,
        embedding_max_daily_usd = config.voyage.max_daily_usd,
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

/// The resolved `[curation]` (ranking included), `[editorial]`, `[voyage]`,
/// model names and prompt versions written to `runs.config_json` (§7.6, §19).
/// Never includes keys.
fn resolved_run_config(config: &Config, soft_target: usize, hard_max: usize) -> serde_json::Value {
    let mut curation = config.curation.clone();
    curation.max_article_count = hard_max;
    let mut voyage = config.voyage.clone();
    voyage.api_key = None;
    serde_json::json!({
        "target_article_count": soft_target,
        "prefilter_keep": config.prefilter_keep,
        "curation": curation,
        "editorial": config.editorial,
        "voyage": voyage,
        "models": {
            "bulk": config.deepseek.model,
            "editor": if config.anthropic.enabled { config.anthropic.model.as_str() } else { "disabled" },
            "editor_effort": config.anthropic.effort,
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
        config.anthropic.api_key = Some("sk-secret".into());
        config.deepseek.api_key = Some("ds-secret".into());
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

    use std::sync::Arc;

    use crate::curate::embedding::{EmbeddingClient, EmbeddingService, MockBackend};
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

        let features = prepare_features(&ctx, &h.articles, &service, &mut report).await;
        let [a, b, blocked, published] = [
            h.articles[0].id,
            h.articles[1].id,
            h.articles[2].id,
            h.articles[3].id,
        ];
        assert_eq!(
            features.keys().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([a, b])
        );
        assert_eq!(report.counts.eligible, 2);
        assert_eq!(report.counts.embedded, 2);
        assert_eq!(report.counts.rated_with_embeddings, 0);
        assert!(report.timings.0.contains_key("embed") && report.timings.0.contains_key("signals"));
        assert!(report.voyage_tokens > 0);
        // One batch for the two articles, one for the interest.
        assert_eq!(backend.calls(), 2);
        let signals = &features[&a].signals;
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

        // The old prefilter and selector, with the stage transitions of step 3.
        let curator = Curator::new(h.config.clone(), h.db.clone(), Llms::default());
        let candidates = curator
            .prefilter(h.articles.clone(), run_date())
            .await
            .unwrap();
        let admitted = candidates.iter().map(|c| c.article.id).collect::<Vec<_>>();
        assert_eq!(
            admitted.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([a, b])
        );
        record_stage(&ctx, &features, &admitted, "admitted", None)
            .await
            .unwrap();
        record_stage(&ctx, &features, &admitted, "shortlisted", None)
            .await
            .unwrap();
        let lineup = curator.select(candidates, run_date()).await.unwrap();
        let selected = lineup
            .picks
            .iter()
            .map(|p| p.article.id)
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), 1);
        let not_selected = admitted
            .iter()
            .copied()
            .filter(|id| !selected.contains(id))
            .collect::<Vec<_>>();
        record_stage(&ctx, &features, &selected, "selected", None)
            .await
            .unwrap();
        record_stage(
            &ctx,
            &features,
            &not_selected,
            "shortlisted",
            Some("not_selected"),
        )
        .await
        .unwrap();

        let rows = stage_rows(&h.db, h.run_id).await;
        assert_eq!(rows.len(), 4);
        let (winner, loser) = (selected[0], not_selected[0]);
        assert_eq!(
            rows[&winner],
            ("selected".into(), None, Some("[\"prefilter\"]".into()))
        );
        assert_eq!(
            rows[&loser],
            (
                "shortlisted".into(),
                Some("not_selected".into()),
                Some("[\"prefilter\"]".into())
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
    }

    #[tokio::test]
    async fn skip_embeddings_makes_zero_voyage_calls_and_uses_the_cache() {
        let h = harness().await;
        let ctx = context(&h, true);
        let mut report = RunReport::new(run_date(), now());
        let service = build_embedding_service(&ctx, &mut report);
        assert!(!service.has_client(), "--skip-embeddings is cache-only");
        assert!(service.meter().is_none());
        let features = prepare_features(&ctx, &h.articles, &service, &mut report).await;
        assert_eq!(features.len(), 2);
        assert_eq!(report.counts.embedded, 0, "nothing cached yet");
        assert!(features.values().all(|f| f.signals.interest.is_none()));
        assert!(features.values().all(|f| f.signals.heuristic.is_some()));
        assert_eq!(report.voyage_tokens, 0);
    }

    #[tokio::test]
    async fn a_voyage_failure_degrades_to_absent_signals_and_the_run_continues() {
        let h = harness().await;
        let ctx = context(&h, false);
        let backend = Arc::new(MockBackend::new()); // nothing scripted: every call fails
        let service = mock_service(&h, backend.clone());
        let mut report = RunReport::new(run_date(), now());
        let features = prepare_features(&ctx, &h.articles, &service, &mut report).await;
        assert!(backend.calls() >= 1);
        assert_eq!(features.len(), 2);
        assert_eq!(report.counts.eligible, 2);
        assert_eq!(report.counts.embedded, 0);
        assert!(report.error.is_none());
        for feature in features.values() {
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
