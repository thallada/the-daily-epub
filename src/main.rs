//! `daily-epub` — CLI entry point (spec §2).
//!
//! Everything of substance lives in the library (`src/lib.rs`); this binary only
//! parses flags, loads config, opens the database and dispatches.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use daily_epub::config::Config;
use daily_epub::curate::embedding::{self, BACKFILL_CONFIRM_TOKENS};
use daily_epub::curate::telemetry;
use daily_epub::db::Db;
use daily_epub::pipeline::{self, GenerateOptions, GenerateOutcome};
use daily_epub::report::{RunReport, VOYAGE_PROVIDER};
use daily_epub::types::{ArticleId, RatingEvent, Vote};
use daily_epub::{curate, http, lock, server, social};

/// A personalized daily newspaper, delivered as an EPUB.
#[derive(Debug, Parser)]
#[command(name = "daily-epub", version, about, long_about = None)]
struct Cli {
    /// Config file path (defaults to ./config.toml when present).
    #[arg(long, short, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build (and publish) one issue.
    Generate(GenerateArgs),
    /// Run the rating endpoints, the OPDS catalog and downloads.
    Serve,
    /// Taste-profile maintenance.
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Inspect and edit explicit article verdicts.
    #[command(subcommand)]
    Ratings(RatingsCommand),
    /// Why an article was (not) in the paper, from persisted run telemetry.
    Explain(ExplainArgs),
    /// The weekly numbers: issues, ratings, retriever yield, cost, timing.
    Stats(StatsArgs),
    /// Embedding cache and telemetry maintenance.
    #[command(subcommand)]
    Features(FeaturesCommand),
    /// Re-poll social scores for recent entries.
    BackfillSocial(BackfillSocialArgs),
    /// Database maintenance.
    #[command(subcommand)]
    Db(DbCommand),
}

#[derive(Debug, clap::Args)]
struct GenerateArgs {
    /// Issue date in the configured timezone (defaults to today).
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: Option<String>,
    /// Build everything but publish nothing: no BookOrbit copy, no issue record.
    #[arg(long)]
    dry_run: bool,
    /// Write artifacts here instead of `out_dir`.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Cap the lineup size (overrides `target_article_count`).
    #[arg(long, value_name = "N")]
    max_articles: Option<usize>,
    /// Skip every LLM call: cheap-signal admission, excerpt summaries.
    #[arg(long)]
    skip_llm: bool,
    /// Use cached embeddings only: zero Voyage calls.
    #[arg(long)]
    skip_embeddings: bool,
    /// Ignore reusable triage/deep assessments and ask the bulk model again.
    #[arg(long)]
    rescore: bool,
}

impl From<&GenerateArgs> for GenerateOptions {
    fn from(args: &GenerateArgs) -> Self {
        Self {
            date: args.date.clone(),
            dry_run: args.dry_run,
            out: args.out.clone(),
            max_articles: args.max_articles,
            skip_llm: args.skip_llm,
            skip_embeddings: args.skip_embeddings,
            rescore: args.rescore,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// Regenerate the taste profile from ratings history.
    Rebuild,
}

#[derive(Debug, Subcommand)]
enum RatingsCommand {
    /// List current explicit ratings, newest first.
    List(RatingsListArgs),
    /// Set or correct an article's explicit rating.
    Set(RatingsSetArgs),
    /// Clear an article from the learned rating set.
    Clear(RatingsClearArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RatingListLabel {
    Loved,
    Good,
    Down,
    Cleared,
}

impl RatingListLabel {
    fn event_label(self) -> &'static str {
        match self {
            Self::Loved => "loved",
            Self::Good => "good",
            Self::Down => "not_for_me",
            Self::Cleared => "cleared",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RatingSetLabel {
    Loved,
    Good,
    Down,
}

impl RatingSetLabel {
    fn vote(self) -> Vote {
        match self {
            Self::Loved => Vote::Loved,
            Self::Good => Vote::Good,
            Self::Down => Vote::NotForMe,
        }
    }
}

#[derive(Debug, clap::Args)]
struct RatingsListArgs {
    /// How many days of verdicts to list.
    #[arg(long, default_value_t = 90)]
    days: i64,
    /// Only verdicts with this label.
    #[arg(long, value_enum)]
    label: Option<RatingListLabel>,
}

#[derive(Debug, clap::Args)]
struct RatingsSetArgs {
    /// Article id, as printed by `ratings list` or `explain`.
    #[arg(long, required_unless_present = "url", conflicts_with = "url")]
    article: Option<ArticleId>,
    /// Article URL; canonicalized before lookup.
    #[arg(long, required_unless_present = "article", conflicts_with = "article")]
    url: Option<String>,
    /// The verdict to record.
    #[arg(long, value_enum)]
    label: RatingSetLabel,
    /// Free-text note shown to the weekly profile rebuild.
    #[arg(long)]
    note: Option<String>,
}

#[derive(Debug, clap::Args)]
struct RatingsClearArgs {
    /// Article id, as printed by `ratings list` or `explain`.
    #[arg(long, required_unless_present = "url", conflicts_with = "url")]
    article: Option<ArticleId>,
    /// Article URL; canonicalized before lookup.
    #[arg(long, required_unless_present = "article", conflicts_with = "article")]
    url: Option<String>,
}

/// `explain --date D (--article ID | --url URL) [--run-id N]` or
/// `explain --date D --near-misses [N]` (plan §15.2).
#[derive(Debug, clap::Args)]
struct ExplainArgs {
    /// Issue date whose run to read.
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: String,
    /// Article id, as printed by `ratings list` or `explain --near-misses`.
    #[arg(
        long,
        required_unless_present_any = ["url", "near_misses"],
        conflicts_with_all = ["url", "near_misses"]
    )]
    article: Option<ArticleId>,
    /// Article URL; canonicalized before lookup.
    #[arg(long, conflicts_with = "near_misses")]
    url: Option<String>,
    /// A specific run of that date instead of the latest non-dry one.
    #[arg(long, value_name = "N")]
    run_id: Option<i64>,
    /// The top N articles that were considered but not selected (default 10).
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "10")]
    near_misses: Option<usize>,
}

/// `stats [--days 14]` (plan §15.3).
#[derive(Debug, clap::Args)]
struct StatsArgs {
    /// How many days back to summarize.
    #[arg(long, default_value_t = 14)]
    days: i64,
}

#[derive(Debug, Subcommand)]
enum FeaturesCommand {
    /// Embed rated and published articles, then interests, into the cache.
    Backfill(BackfillArgs),
    /// Drop stale embeddings, old candidate telemetry and old assessments per the retention config.
    Prune,
}

#[derive(Debug, clap::Args)]
struct BackfillArgs {
    /// Window for published (and, with --all, other) articles.
    #[arg(long, default_value_t = 30)]
    days: i64,
    /// Only the rated set.
    #[arg(long, conflicts_with = "all")]
    rated_only: bool,
    /// Also every other article first seen inside the window.
    #[arg(long)]
    all: bool,
    /// Skip the confirmation prompt above the token threshold.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, clap::Args)]
struct BackfillSocialArgs {
    /// How many days back to re-poll.
    #[arg(long, default_value_t = 7)]
    days: u32,
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Run pending sqlx migrations.
    Migrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("loading configuration")?;
    tracing::debug!(?config.database_path, "configuration loaded");

    // One writer at a time (§5); read-only commands never wait on it.
    let _lock = match lock_holder(&cli.command) {
        Some(name) => {
            Some(lock::acquire(&config.database_path, name).map_err(|e| anyhow::anyhow!("{e}"))?)
        }
        None => None,
    };

    match cli.command {
        Command::Generate(args) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            let outcome = pipeline::generate(&config, &db, &GenerateOptions::from(&args)).await?;
            print_outcome(&outcome);
        }
        Command::Serve => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            server::serve(config, db).await?;
        }
        Command::Profile(ProfileCommand::Rebuild) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            cmd_profile_rebuild(&config, &db).await?;
        }
        Command::Ratings(command) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            cmd_ratings(&config, &db, command).await?;
        }
        Command::Explain(args) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            cmd_explain(&db, args).await?;
        }
        Command::Stats(args) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            let text = telemetry::stats(&db, args.days, jiff::Timestamp::now()).await?;
            print!("{text}");
        }
        Command::Features(command) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            cmd_features(&config, &db, command).await?;
        }
        Command::BackfillSocial(args) => {
            let db = Db::open_and_migrate(&config.database_path).await?;
            cmd_backfill_social(&db, args.days).await?;
        }
        Command::Db(DbCommand::Migrate) => {
            let db = Db::open(&config.database_path).await?;
            db.migrate().await?;
            println!("migrations up to date: {}", config.database_path.display());
        }
    }
    Ok(())
}

/// The commands that write the database and provider budgets and so hold the
/// run lock (§5): `generate`, `profile rebuild`, `features backfill`,
/// `backfill-social`. Everything else is read-only or its own writer.
fn lock_holder(command: &Command) -> Option<&'static str> {
    match command {
        Command::Generate(_) => Some("generate"),
        Command::Profile(ProfileCommand::Rebuild) => Some("profile rebuild"),
        Command::Features(FeaturesCommand::Backfill(_)) => Some("features backfill"),
        Command::BackfillSocial(_) => Some("backfill-social"),
        Command::Serve
        | Command::Ratings(_)
        | Command::Explain(_)
        | Command::Stats(_)
        | Command::Features(FeaturesCommand::Prune)
        | Command::Db(_) => None,
    }
}

/// `RUST_LOG`-driven tracing, defaulting to `info` (crate table "logging").
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,hyper=warn,reqwest=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Human-readable end-of-run output; the machine-readable form lives in `runs`
/// and in the issue's `report_json` (§3.13).
fn print_outcome(outcome: &GenerateOutcome) {
    print_report(&outcome.report);
    if let Some(issue) = &outcome.issue {
        print_lineup(issue);
    }
    for artifact in &outcome.artifacts {
        println!(
            "built:  {} ({:.1} MiB)",
            artifact.path.display(),
            artifact.bytes as f64 / (1024.0 * 1024.0)
        );
    }
    if let Some(xtc) = &outcome.xtc {
        println!("xtc:    {}", xtc.display());
    }
    match &outcome.published {
        Some(published) => {
            for artifact in &published.epubs {
                println!("published: {}", artifact.path.display());
            }
            if let Some(xtc) = &published.xtc {
                println!("published: {}", xtc.display());
            }
            if published.pruned > 0 {
                println!("pruned:    {} expired files", published.pruned);
            }
        }
        None => println!("dry run: nothing was published"),
    }
}

fn print_report(report: &RunReport) {
    println!("{}", report.summary_line());
    if let (Some(start), Some(end)) = (report.window_start, report.window_end) {
        println!("window: {start} → {end}");
    }
    println!(
        "entries: {} from {} feeds → {} articles ({} merged, {} dropped)",
        report.counts.entries_fetched,
        report.counts.feeds_seen,
        report.counts.articles,
        report.counts.duplicates_merged,
        report.counts.entries_dropped,
    );
    // The same four lines the run logged (§15.4).
    for line in report.info_block() {
        println!("{line}");
    }
    println!(
        "tokens: {} input · {} cache read · {} cache write · {} output · {} voyage = ${:.4}",
        report.usage.input_tokens,
        report.usage.cached_tokens,
        report.usage.cache_write_tokens,
        report.usage.output_tokens,
        report.voyage_tokens,
        report.cost_usd,
    );
    for (provider, usage) in &report.provider_costs {
        if provider == VOYAGE_PROVIDER {
            continue; // embedding tokens are printed on their own line below
        }
        println!(
            "  {provider}: {} input · {} cache read · {} cache write · {} output = ${:.4}",
            usage.usage.input_tokens,
            usage.usage.cached_tokens,
            usage.usage.cache_write_tokens,
            usage.usage.output_tokens,
            usage.cost_usd,
        );
    }
    println!(
        "  voyage: {} tokens = ${:.4}",
        report.voyage_tokens, report.voyage_cost_usd
    );
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    if let Some(err) = &report.error {
        println!("error: {err}");
    }
    tracing::debug!("{}", report.to_json());
}

/// The day's lineup, section by section — the `--dry-run` deliverable (§4 M3).
fn print_lineup(issue: &daily_epub::types::Issue) {
    println!(
        "\nThe Daily EPUB No. {} — {} · {}",
        issue.meta.issue_number,
        issue.meta.display_date,
        issue.meta.stats_line()
    );
    for section in &issue.lineup.section_order {
        println!("\n  {section}");
        for pick in issue.lineup.section_picks(section) {
            let lead = if pick.is_lead { "★ " } else { "  " };
            println!(
                "  {lead}{} — {} ({} min)",
                pick.article.title,
                pick.article.feed_title,
                pick.article.reading_minutes()
            );
        }
    }
    if issue.world_briefing.is_some() {
        println!("\n  {}", daily_epub::types::WORLD_BRIEFING_SECTION);
    }
    println!();
}

// ---------------------------------------------------------------------------
// Other subcommands
// ---------------------------------------------------------------------------

/// `profile rebuild` runs on the editor when configured, else bulk (§14.3).
async fn cmd_profile_rebuild(config: &Config, db: &Db) -> Result<()> {
    use curate::llm::{Llms, PriceTable, UsageMeter};
    let bulk_meter =
        UsageMeter::with_prices(PriceTable::deepseek(&config.deepseek), config.max_daily_usd);
    let editor_meter = UsageMeter::with_prices(
        PriceTable::anthropic(&config.anthropic),
        config.anthropic.max_daily_usd,
    );
    let profile = curate::profile::load_or_build(
        db,
        &config.interests_opml,
        &config.profile_path,
        config.curation.feedback.verdicts_in_prompt,
    )
    .await?;
    let llms = Llms::from_config(
        &config.deepseek,
        &config.anthropic,
        profile.text,
        bulk_meter,
        editor_meter,
    );
    let Some(llm) = llms.editor_or_bulk() else {
        anyhow::bail!(
            "no LLM provider is configured; set DAILY_EPUB_ANTHROPIC__API_KEY or DAILY_EPUB_DEEPSEEK__API_KEY"
        );
    };
    tracing::info!(provider = llm.provider, model = %llm.model, "rebuilding the profile");
    let rebuilt = curate::profile::rebuild(
        db,
        llm,
        &config.interests_opml,
        &config.profile_path,
        config.curation.feedback.verdicts_in_prompt,
    )
    .await?;
    println!(
        "taste profile rebuilt (version {}, {} chars)",
        rebuilt.version,
        rebuilt.text.len()
    );
    Ok(())
}

async fn resolve_rating_article(
    db: &Db,
    article: Option<ArticleId>,
    url: Option<&str>,
) -> Result<ArticleId> {
    let article_id = match (article, url) {
        (Some(article_id), None) => article_id,
        (None, Some(url)) => {
            let canonical = daily_epub::dedupe::canonical_url(url)
                .with_context(|| format!("invalid article URL {url:?}"))?;
            db.article_id_for_url(&canonical)
                .await?
                .with_context(|| format!("no article found for {canonical}"))?
        }
        _ => anyhow::bail!("provide exactly one of --article or --url"),
    };
    if db.get_article(article_id).await?.is_none() {
        anyhow::bail!("article {article_id} was not found");
    }
    Ok(article_id)
}

async fn append_cli_event(
    config: &Config,
    db: &Db,
    article_id: ArticleId,
    vote: Option<Vote>,
    note: Option<String>,
) -> Result<i64> {
    let (label, value) = match vote {
        Some(Vote::Loved) => ("loved", Vote::Loved.value(&config.curation.feedback)),
        Some(Vote::Good) => ("good", Vote::Good.value(&config.curation.feedback)),
        Some(Vote::NotForMe) => (
            "not_for_me",
            Vote::NotForMe.value(&config.curation.feedback),
        ),
        None => ("cleared", 0.0),
    };
    let event = RatingEvent {
        id: 0,
        article_id,
        issue_date: db.latest_issue_date_for_article(article_id).await?,
        kind: "explicit".into(),
        source: "cli".into(),
        label: label.into(),
        value,
        note,
        event_at: jiff::Timestamp::now(),
    };
    Ok(db.append_rating_event(&event).await?)
}

async fn cmd_ratings(config: &Config, db: &Db, command: RatingsCommand) -> Result<()> {
    match command {
        RatingsCommand::List(args) => {
            let ratings = db.current_ratings_including_cleared(args.days).await?;
            let mut shown = 0usize;
            for rating in ratings {
                if args
                    .label
                    .is_some_and(|label| rating.label != label.event_label())
                {
                    continue;
                }
                let note = rating
                    .note
                    .as_deref()
                    .map(|note| format!(" · note: {note}"))
                    .unwrap_or_default();
                println!(
                    "{} · article {} · {} · {} — {}{}",
                    rating.event_at,
                    rating.article_id,
                    rating.label,
                    rating.title,
                    rating.feed_title,
                    note
                );
                shown += 1;
            }
            println!("{shown} current rating(s)");
        }
        RatingsCommand::Set(args) => {
            let article_id = resolve_rating_article(db, args.article, args.url.as_deref()).await?;
            let vote = args.label.vote();
            let event_id = append_cli_event(config, db, article_id, Some(vote), args.note).await?;
            println!(
                "recorded {} for article {article_id} (event {event_id})",
                vote.as_str()
            );
        }
        RatingsCommand::Clear(args) => {
            let article_id = resolve_rating_article(db, args.article, args.url.as_deref()).await?;
            let event_id = append_cli_event(config, db, article_id, None, None).await?;
            println!("cleared article {article_id} (event {event_id})");
        }
    }
    Ok(())
}

async fn cmd_explain(db: &Db, args: ExplainArgs) -> Result<()> {
    let date: jiff::civil::Date = args
        .date
        .parse()
        .with_context(|| format!("invalid --date {:?}, expected YYYY-MM-DD", args.date))?;
    let text = if let Some(limit) = args.near_misses {
        telemetry::explain_near_misses(db, date, args.run_id, limit).await?
    } else {
        let target = match (args.article, args.url) {
            (Some(id), _) => telemetry::ExplainTarget::Article(id),
            (None, Some(url)) => telemetry::ExplainTarget::Url(url),
            (None, None) => anyhow::bail!("provide --article, --url or --near-misses"),
        };
        telemetry::explain(db, date, args.run_id, &target).await?
    };
    print!("{text}");
    Ok(())
}

async fn cmd_features(config: &Config, db: &Db, command: FeaturesCommand) -> Result<()> {
    match command {
        FeaturesCommand::Backfill(args) => {
            if !config.voyage.enabled {
                anyhow::bail!("voyage.enabled is false; nothing to backfill");
            }
            let service = embedding::EmbeddingService::real(db.clone(), config.voyage.clone())
                .context("building the Voyage client")?;
            let opts = embedding::BackfillOptions {
                days: args.days,
                rated_only: args.rated_only,
                all: args.all,
            };
            let plan = embedding::plan_backfill(db, config, &service, &opts).await?;
            println!(
                "backfill: {} articles ({} learned, {} other) + {} interests to embed, {} already cached",
                plan.article_count(),
                plan.learned.len(),
                plan.others.len(),
                plan.interests.len(),
                plan.cached
            );
            if plan.is_empty() {
                println!("cache is warm; nothing to do");
                return Ok(());
            }
            println!(
                "estimate: ~{} tokens ≈ ${:.4} with {} at ${:.2}/M",
                plan.estimated_tokens,
                plan.estimated_cost_usd(),
                config.voyage.model,
                embedding::VOYAGE_PRICE_PER_MTOK
            );
            if plan.estimated_tokens > BACKFILL_CONFIRM_TOKENS
                && !args.yes
                && !confirm("continue?")?
            {
                println!("aborted");
                return Ok(());
            }
            let outcome = embedding::run_backfill(&service, &plan).await?;
            println!(
                "embedded {} articles and {} interests · {} tokens · ${:.4}",
                outcome.articles_embedded,
                outcome.interests_embedded,
                outcome.tokens,
                outcome.cost_usd
            );
        }
        FeaturesCommand::Prune => {
            let ranking = &config.curation.ranking;
            let pruned = telemetry::prune(
                db,
                ranking.embedding_retention_days,
                ranking.telemetry_retention_days,
                jiff::Timestamp::now(),
            )
            .await?;
            println!(
                "pruned {} embeddings older than {} days, {} candidate rows and {} assessments older than {} days",
                pruned.embeddings,
                ranking.embedding_retention_days,
                pruned.telemetry,
                pruned.assessments,
                ranking.telemetry_retention_days
            );
        }
    }
    Ok(())
}

/// A y/N question on stdin; anything but a leading `y` is a no.
fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_lowercase().starts_with('y'))
}

async fn cmd_backfill_social(db: &Db, days: u32) -> Result<()> {
    let http = http::build_client(http::DEFAULT_TIMEOUT)?;
    let enricher = social::SocialEnricher::new(http, db.clone());
    let updated = enricher.backfill(days).await?;
    println!("refreshed social scores for {updated} articles");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_every_subcommand_from_the_spec() {
        let cli = Cli::try_parse_from([
            "daily-epub",
            "generate",
            "--date",
            "2026-08-15",
            "--dry-run",
            "--out",
            "./out",
            "--max-articles",
            "6",
            "--skip-llm",
            "--skip-embeddings",
            "--rescore",
        ])
        .unwrap();
        match cli.command {
            Command::Generate(a) => {
                assert_eq!(a.date.as_deref(), Some("2026-08-15"));
                assert!(a.dry_run);
                assert_eq!(a.out, Some(PathBuf::from("./out")));
                assert_eq!(a.max_articles, Some(6));
                assert!(a.skip_llm);
                assert!(a.skip_embeddings);
                assert!(a.rescore);

                let opts = GenerateOptions::from(&a);
                assert_eq!(opts.date.as_deref(), Some("2026-08-15"));
                assert!(opts.dry_run && opts.skip_llm && opts.skip_embeddings && opts.rescore);
                assert_eq!(opts.max_articles, Some(6));
            }
            other => panic!("expected generate, got {other:?}"),
        }

        assert!(matches!(
            Cli::try_parse_from(["daily-epub", "serve"])
                .unwrap()
                .command,
            Command::Serve
        ));
        assert!(matches!(
            Cli::try_parse_from(["daily-epub", "profile", "rebuild"])
                .unwrap()
                .command,
            Command::Profile(ProfileCommand::Rebuild)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "daily-epub",
                "ratings",
                "set",
                "--article",
                "42",
                "--label",
                "good"
            ])
            .unwrap()
            .command,
            Command::Ratings(RatingsCommand::Set(_))
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "daily-epub",
                "ratings",
                "clear",
                "--url",
                "https://example.com"
            ])
            .unwrap()
            .command,
            Command::Ratings(RatingsCommand::Clear(_))
        ));

        assert!(matches!(
            Cli::try_parse_from(["daily-epub", "backfill-social", "--days", "14"])
                .unwrap()
                .command,
            Command::BackfillSocial(BackfillSocialArgs { days: 14 })
        ));
        assert!(matches!(
            Cli::try_parse_from(["daily-epub", "db", "migrate"])
                .unwrap()
                .command,
            Command::Db(DbCommand::Migrate)
        ));

        let cli = Cli::try_parse_from(["daily-epub", "--config", "/tmp/x.toml", "serve"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn only_the_writing_commands_take_the_lock() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("daily-epub").chain(args.iter().copied()))
                .unwrap()
                .command
        };
        assert_eq!(lock_holder(&parse(&["generate"])), Some("generate"));
        assert_eq!(
            lock_holder(&parse(&["profile", "rebuild"])),
            Some("profile rebuild")
        );
        assert_eq!(
            lock_holder(&parse(&["features", "backfill"])),
            Some("features backfill")
        );
        assert_eq!(
            lock_holder(&parse(&["backfill-social"])),
            Some("backfill-social")
        );
        for args in [
            vec!["serve"],
            vec!["explain", "--date", "2026-09-02", "--near-misses"],
            vec!["stats"],
            vec!["ratings", "list"],
            vec!["db", "migrate"],
            vec!["features", "prune"],
        ] {
            assert_eq!(lock_holder(&parse(&args)), None, "{args:?}");
        }
    }

    #[test]
    fn parses_stats() {
        match Cli::try_parse_from(["daily-epub", "stats"])
            .unwrap()
            .command
        {
            Command::Stats(args) => assert_eq!(args.days, 14),
            other => panic!("expected stats, got {other:?}"),
        }
        match Cli::try_parse_from(["daily-epub", "stats", "--days", "7"])
            .unwrap()
            .command
        {
            Command::Stats(args) => assert_eq!(args.days, 7),
            other => panic!("expected stats, got {other:?}"),
        }
    }

    #[test]
    fn parses_explain_and_features() {
        match Cli::try_parse_from([
            "daily-epub",
            "explain",
            "--date",
            "2026-09-02",
            "--article",
            "42",
            "--run-id",
            "7",
        ])
        .unwrap()
        .command
        {
            Command::Explain(args) => {
                assert_eq!(args.date, "2026-09-02");
                assert_eq!(args.article, Some(42));
                assert_eq!(args.run_id, Some(7));
                assert_eq!(args.near_misses, None);
            }
            other => panic!("expected explain, got {other:?}"),
        }
        match Cli::try_parse_from([
            "daily-epub",
            "explain",
            "--date",
            "2026-09-02",
            "--url",
            "https://example.com/post",
        ])
        .unwrap()
        .command
        {
            Command::Explain(args) => {
                assert_eq!(args.url.as_deref(), Some("https://example.com/post"))
            }
            other => panic!("expected explain, got {other:?}"),
        }
        match Cli::try_parse_from([
            "daily-epub",
            "explain",
            "--date",
            "2026-09-02",
            "--near-misses",
        ])
        .unwrap()
        .command
        {
            Command::Explain(args) => assert_eq!(args.near_misses, Some(10)),
            other => panic!("expected explain, got {other:?}"),
        }
        match Cli::try_parse_from([
            "daily-epub",
            "explain",
            "--date",
            "2026-09-02",
            "--near-misses",
            "3",
        ])
        .unwrap()
        .command
        {
            Command::Explain(args) => assert_eq!(args.near_misses, Some(3)),
            other => panic!("expected explain, got {other:?}"),
        }
        assert!(Cli::try_parse_from(["daily-epub", "explain", "--date", "2026-09-02"]).is_err());
        assert!(
            Cli::try_parse_from([
                "daily-epub",
                "explain",
                "--date",
                "2026-09-02",
                "--article",
                "1",
                "--near-misses"
            ])
            .is_err()
        );

        match Cli::try_parse_from([
            "daily-epub",
            "features",
            "backfill",
            "--days",
            "60",
            "--all",
            "--yes",
        ])
        .unwrap()
        .command
        {
            Command::Features(FeaturesCommand::Backfill(args)) => {
                assert_eq!(args.days, 60);
                assert!(args.all && args.yes && !args.rated_only);
            }
            other => panic!("expected features backfill, got {other:?}"),
        }
        match Cli::try_parse_from(["daily-epub", "features", "backfill"])
            .unwrap()
            .command
        {
            Command::Features(FeaturesCommand::Backfill(args)) => assert_eq!(args.days, 30),
            other => panic!("expected features backfill, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from([
                "daily-epub",
                "features",
                "backfill",
                "--rated-only",
                "--all"
            ])
            .is_err()
        );
        assert!(matches!(
            Cli::try_parse_from(["daily-epub", "features", "prune"])
                .unwrap()
                .command,
            Command::Features(FeaturesCommand::Prune)
        ));
    }

    #[tokio::test]
    async fn cli_set_and_clear_append_cli_events_with_latest_issue_date() {
        use sqlx::Row as _;

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("ratings.db"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
             (42, 'https://example.com/article', 'Article', '2026-08-15T00:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at) VALUES
             ('2026-08-14', 1, '2026-08-14T12:00:00Z'),
             ('2026-08-15', 2, '2026-08-15T12:00:00Z')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO issue_articles (issue_date, article_id, section) VALUES
             ('2026-08-14', 42, 'Top Stories'),
             ('2026-08-15', 42, 'Top Stories')",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let config = Config::default();
        append_cli_event(
            &config,
            &db,
            42,
            Some(Vote::Good),
            Some("useful note".into()),
        )
        .await
        .unwrap();
        append_cli_event(&config, &db, 42, None, None)
            .await
            .unwrap();

        let rows = sqlx::query(
            "SELECT issue_date, source, label, value, note FROM rating_events ORDER BY id",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<String, _>("source"), "cli");
        assert_eq!(rows[0].get::<String, _>("issue_date"), "2026-08-15");
        assert_eq!(rows[0].get::<String, _>("label"), "good");
        assert_eq!(rows[0].get::<f64, _>("value"), 0.35);
        assert_eq!(rows[0].get::<String, _>("note"), "useful note");
        assert_eq!(rows[1].get::<String, _>("source"), "cli");
        assert_eq!(rows[1].get::<String, _>("label"), "cleared");
        assert_eq!(rows[1].get::<f64, _>("value"), 0.0);
        assert!(db.current_ratings(36500).await.unwrap().is_empty());
    }
}
