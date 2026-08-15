//! `daily-epub` — CLI entry point (spec §2).
//!
//! Everything of substance lives in the library (`src/lib.rs`); this binary only
//! parses flags, loads config, opens the database and dispatches.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use daily_epub::config::Config;
use daily_epub::db::Db;
use daily_epub::pipeline::{self, GenerateOptions, GenerateOutcome};
use daily_epub::report::RunReport;
use daily_epub::{curate, http, server, social};

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
    /// Skip every LLM call: prefilter order selects, excerpts stand in for summaries.
    #[arg(long)]
    skip_llm: bool,
}

impl From<&GenerateArgs> for GenerateOptions {
    fn from(args: &GenerateArgs) -> Self {
        Self {
            date: args.date.clone(),
            dry_run: args.dry_run,
            out: args.out.clone(),
            max_articles: args.max_articles,
            skip_llm: args.skip_llm,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// Regenerate the taste profile from ratings history.
    Rebuild,
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
    println!(
        "tokens: {} input · {} cached · {} output = ${:.4}",
        report.usage.input_tokens,
        report.usage.cached_tokens,
        report.usage.output_tokens,
        report.cost_usd,
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

async fn cmd_profile_rebuild(config: &Config, db: &Db) -> Result<()> {
    let meter = curate::llm::UsageMeter::new(&config.deepseek, config.max_daily_usd);
    let profile = curate::profile::load_or_build(db, &config.interests_opml).await?;
    let llm = curate::llm::LlmClient::new(&config.deepseek, profile.text, meter)?;
    let rebuilt = curate::profile::rebuild(db, &llm, &config.interests_opml).await?;
    let feeds = curate::profile::rebuild_feed_priors(db).await?;
    println!(
        "taste profile rebuilt (version {}, {} chars); {feeds} feed priors refreshed",
        rebuilt.version,
        rebuilt.text.len()
    );
    Ok(())
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
        ])
        .unwrap();
        match cli.command {
            Command::Generate(a) => {
                assert_eq!(a.date.as_deref(), Some("2026-08-15"));
                assert!(a.dry_run);
                assert_eq!(a.out, Some(PathBuf::from("./out")));
                assert_eq!(a.max_articles, Some(6));
                assert!(a.skip_llm);

                let opts = GenerateOptions::from(&a);
                assert_eq!(opts.date.as_deref(), Some("2026-08-15"));
                assert!(opts.dry_run && opts.skip_llm);
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
}
