//! Seed a throwaway development database so the web site can be browsed
//! locally with realistic data — two issues (one with the full `issue_json`
//! snapshot, one "legacy" issue without it), their runs, an admin and a
//! reader account. For design work and manual smoke tests only; never run it
//! against the production database.
//!
//! ```text
//! cargo run --example seed_dev_db -- ./dev
//! DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) \
//!     cargo run -- --config ./dev/config.toml serve
//! ```
//!
//! Then open http://127.0.0.1:3599 and sign in as `admin` / `adminpassword123`
//! or `reader` / `readerpassword123`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use daily_epub::db::Db;
use daily_epub::epub::fixtures;
use daily_epub::report::{RunReport, RunStatus};
use daily_epub::types::{Article, Entry, Issue, Pick};
use daily_epub::web::users;
use jiff::Timestamp;
use jiff::civil::Date;

const ADMIN_PASSWORD: &str = "adminpassword123";
const READER_PASSWORD: &str = "readerpassword123";

/// (section, title, feed, summary, why, words)
const STORIES: &[(&str, &str, &str, &str, &str, i64)] = &[
    (
        "Top Stories",
        "How a Forty-Year-Old Filesystem Quietly Rewrote Its Write Path",
        "Systems Weekly",
        "A long, careful account of the redesign, with the benchmarks that justified it and the two regressions that nearly sank it.",
        "The systems story with enough operational detail to matter to you",
        3100,
    ),
    (
        "Top Stories",
        "The Case Against Feature Flags",
        "Alice on Software",
        "Argues that flags outlive their purpose and proposes a retirement discipline, with examples from three codebases.",
        "A contrarian take on tooling you use every day",
        1850,
    ),
    (
        "Top Stories",
        "What the Latest Battery Chemistry Actually Changes",
        "Ars Technica",
        "Separates the press-release claims from the measurable improvements in energy density and cycle life.",
        "You keep an eye on energy storage; this one is unusually sober",
        2200,
    ),
    (
        "Deep Reads",
        "A Field Guide to Distributed Consensus, Told Through One Outage",
        "The Morning Paper",
        "Walks through a real incident to explain leader election, log replication and why the fix was a config change.",
        "Distributed systems explained with an actual outage rather than diagrams",
        4200,
    ),
    (
        "Deep Reads",
        "Why Every Map Is a Lie & How Cartographers Choose Which One to Tell",
        "Longreads",
        "A history of projections and the politics behind them, from Mercator to the maps in your phone.",
        "Long-form nonfiction outside the technical orbit you asked for",
        5100,
    ),
    (
        "Tools & Craft",
        "Notes on Writing a Rust Linter That People Actually Enable",
        "Rust Blog",
        "Design notes on false-positive budgets, fix suggestions and the social side of shipping a lint.",
        "Rust tooling with a practical bent",
        1600,
    ),
    (
        "Tools & Craft",
        "The Terminal Is the Best UI We Have and It Is Getting Better",
        "Julia's Notebook",
        "A tour of modern terminal features (hyperlinks, images, synchronized output) and which tools use them.",
        "Terminal ergonomics, one of your recurring interests",
        1300,
    ),
    (
        "Niche Corner",
        "A Small-Town Bakery's Sourdough Starter Turns One Hundred",
        "Saveur",
        "A charming profile of a starter kept alive across four generations and the bread it still makes.",
        "A small-scene delight outside the usual technical orbit",
        900,
    ),
    (
        "Niche Corner",
        "Inside the Community Keeping 1990s Synthesizers Alive",
        "Sound on Sound",
        "Repair collectives, replacement parts and the odd economics of vintage gear.",
        "Music hardware, for the weekend",
        1700,
    ),
];

fn story_article(index: usize, story: &(&str, &str, &str, &str, &str, i64)) -> Article {
    let id = index as i64 + 1;
    let entry_id = 1000 + id;
    let (_, title, feed, _, _, words) = *story;
    let mut article = fixtures::article(id, entry_id, title);
    article.feed_title = feed.to_string();
    article.feed_id = 7 + index as i64;
    article.word_count = words;
    for source in &mut article.sources {
        source.feed_title = feed.to_string();
        source.feed_id = article.feed_id;
    }
    article.content_html = body_html(title, words);
    if !index.is_multiple_of(3) {
        article.social.clear();
    }
    if index == 4 {
        article.author = None;
    }
    article
}

fn body_html(title: &str, words: i64) -> String {
    let paragraphs = (words / 120).clamp(4, 40);
    let mut html = format!("<p>Body of <em>{title}</em>. ");
    html.push_str("This paragraph exists so the article page has something to lay out: a first sentence with a claim, a second with the evidence, and a third that hedges just enough to be honest.</p>");
    html.push_str("<h2>Where it gets interesting</h2>");
    for index in 0..paragraphs {
        if index == 2 {
            html.push_str("<img src=\"https://picsum.photos/seed/daily-epub/900/500\" alt=\"A chart of the daily figures\">");
        }
        if index == 5 {
            html.push_str("<blockquote><p>A pull quote that the author would rather you remembered than the rest.</p></blockquote>");
        }
        if index == 7 {
            html.push_str(
                "<pre><code>fn main() {\n    println!(\"hello, reader\");\n}</code></pre>",
            );
        }
        html.push_str("<p>Paragraph ");
        html.push_str(&(index + 1).to_string());
        html.push_str(" keeps the argument moving. Long sentences alternate with short ones. Then a list of considerations that the author says nobody weighs properly, followed by the observation that everyone weighs them and simply disagrees about the weights.</p>");
    }
    html.push_str("<ul><li>First consideration</li><li>Second consideration</li><li>A third, longer consideration that wraps onto another line on narrow screens</li></ul>");
    html
}

fn dev_issue(date: Date, issue_number: i64, generated_at: Timestamp) -> Issue {
    let mut issue = fixtures::issue();
    issue.meta.date = date;
    issue.meta.issue_number = issue_number;
    issue.meta.generated_at = generated_at;
    issue.meta.display_date = date.strftime("%A, %B %-d, %Y").to_string();
    issue.lineup.date = date;
    let mut section_order = Vec::new();
    let mut picks = Vec::new();
    let mut summaries = BTreeMap::new();
    for (index, story) in STORIES.iter().enumerate() {
        let (section, _, _, summary, why, _) = *story;
        if !section_order.iter().any(|s| s == section) {
            section_order.push(section.to_string());
        }
        let position = picks
            .iter()
            .filter(|pick: &&Pick| pick.section == section)
            .count() as i64;
        let article = story_article(index, story);
        summaries.insert(article.id, summary.to_string());
        picks.push(Pick {
            discussion: (index == 0)
                .then(|| fixtures::discussion(article.id, article.best_entry_id)),
            article,
            section: section.to_string(),
            position,
            is_lead: index == 0,
            why: Some(why.to_string()),
            summary: Some(summary.to_string()),
            llm: None,
        });
    }
    issue.meta.article_count = picks.len() as i64;
    issue.meta.section_count = section_order.len() as i64;
    issue.meta.total_words = picks.iter().map(|pick| pick.article.word_count).sum();
    issue.meta.reading_minutes = daily_epub::types::reading_minutes(issue.meta.total_words);
    issue.lineup.picks = picks;
    issue.lineup.section_order = section_order;
    issue.editorial.summaries = summaries;
    issue.editorial.front_page_html = "<p>Nine stories today. The filesystem rewrite leads because it is the rare systems piece that shows its benchmarks and its mistakes; the consensus outage walkthrough is the long read to save for the train.</p><p>Two contrarian pieces on tooling sit in the middle, and the back pages are for the weekend: a hundred-year-old sourdough starter and the people who keep old synthesizers humming.</p>".into();
    if let Some(world) = issue.world_briefing.as_mut() {
        world.date = date;
    }
    issue.behind.selected = issue.meta.article_count;
    issue
}

async fn seed_issue(db: &Db, issue: &mut Issue, with_snapshot: bool) -> anyhow::Result<()> {
    for pick in &mut issue.lineup.picks {
        let article = &pick.article;
        db.upsert_entry(&Entry {
            id: article.best_entry_id,
            feed_id: article.feed_id,
            feed_title: Some(article.feed_title.clone()),
            category: article.category.clone(),
            title: article.title.clone(),
            url: article.url.clone(),
            canonical_url: Some(article.canonical_url.clone()),
            author: article.author.clone(),
            published_at: article.published_at,
            comments_url: article.comments_url.clone(),
            raw_content: article.content_html.clone(),
            fetched_at: article.first_seen,
        })
        .await?;
        let id = db.upsert_article(article).await?;
        pick.article.id = id;
        for social in &mut pick.article.social {
            social.article_id = id;
            db.upsert_social(social).await?;
        }
    }
    let snapshot = with_snapshot
        .then(|| {
            let mut snapshot = issue.clone();
            for pick in &mut snapshot.lineup.picks {
                pick.article.content_html.clear();
            }
            serde_json::to_string(&snapshot)
        })
        .transpose()?;
    let run_id = db
        .start_run(issue.meta.date, issue.meta.generated_at)
        .await?;
    let mut report = RunReport::new(issue.meta.date, issue.meta.generated_at);
    report.finished_at = Some(issue.meta.generated_at);
    report.status = RunStatus::Ok;
    report.counts.entries_fetched = issue.colophon.entries_fetched;
    report.counts.feeds_seen = issue.colophon.feeds_seen;
    report.counts.articles = issue.behind.considered;
    report.counts.eligible = issue.behind.eligible;
    report.counts.triaged = issue.behind.triaged;
    report.counts.assessed = issue.behind.read_closely;
    report.counts.shortlisted = issue.behind.shortlisted;
    report.counts.candidates = issue.colophon.candidates;
    report.counts.selected = issue.meta.article_count;
    report.counts.admitted_by = issue.behind.admitted_by.clone();
    report.counts.rated_with_embeddings = issue.behind.rated_with_embeddings;
    report.counts.knn_gate = issue.behind.knn_gate;
    report.counts.feed_gate = issue.behind.feed_gate;
    report.counts.embedded = 300;
    report.cost_usd = issue.colophon.cost_usd;
    report.config_json = serde_json::json!({
        "llm": {"bulk": {"model": issue.colophon.models.bulk}, "editor": {"model": issue.colophon.models.editor}},
        "voyage": {"model": "voyage-4"},
    });
    db.finish_run(run_id, &report).await?;
    db.upsert_issue(
        issue.meta.date,
        issue.meta.issue_number,
        issue.meta.generated_at,
        None,
        None,
        None,
        Some(&issue.editorial.front_page_html),
        Some(&serde_json::to_string(&report)?),
        snapshot.as_deref(),
    )
    .await?;
    db.replace_issue_articles(issue.meta.date, &issue.lineup.picks)
        .await?;
    Ok(())
}

fn write_config(dir: &Path) -> anyhow::Result<PathBuf> {
    let path = dir.join("config.toml");
    let dir = dir.canonicalize()?;
    // The profile lives inside the throwaway directory: saving it from
    // /dashboard/profile must never rewrite the repository's data/profile.md.
    let profile = dir.join("profile.md");
    let toml = format!(
        "database_path = \"{db}\"\nprofile_path = \"{profile}\"\n\n[publish]\nepub_dir = \"{epubs}\"\nxtc_dir = \"{xtc}\"\n\n[server]\nbind = \"127.0.0.1:3599\"\npublic_url = \"http://127.0.0.1:3599\"\njobs_enabled = false\n",
        db = dir.join("daily-epub.db").display(),
        profile = profile.display(),
        epubs = dir.join("epubs").display(),
        xtc = dir.join("xtc").display(),
    );
    std::fs::write(&path, toml)?;
    if !profile.exists() {
        let shipped = Path::new("data/profile.md");
        let body = std::fs::read_to_string(shipped)
            .unwrap_or_else(|_| "# Reader profile\n\n## Who he is\n\nA developer.\n".to_string());
        std::fs::write(&profile, body)?;
    }
    std::fs::create_dir_all(dir.join("epubs"))?;
    std::fs::create_dir_all(dir.join("xtc"))?;
    Ok(path)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dev"));
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("daily-epub.db");
    if db_path.exists() {
        anyhow::bail!("{} already exists; delete it first", db_path.display());
    }
    let config_path = write_config(&dir)?;
    let db = Db::open_and_migrate(&db_path).await?;

    users::add(&db, "admin", ADMIN_PASSWORD, true).await?;
    users::add(&db, "reader", READER_PASSWORD, false).await?;

    // Legacy issue: published before `issue_json` existed, so the site must
    // rebuild the colophon and back matter from the run instead.
    let mut legacy = dev_issue("2026-09-01".parse()?, 18, "2026-09-01T05:31:00Z".parse()?);
    seed_issue(&db, &mut legacy, false).await?;

    let mut latest = dev_issue("2026-09-02".parse()?, 19, "2026-09-02T05:29:00Z".parse()?);
    seed_issue(&db, &mut latest, true).await?;

    println!("seeded {}", db_path.display());
    println!(
        "run:  DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) cargo run -- --config {} serve",
        config_path.display()
    );
    println!("open: http://127.0.0.1:3599  (admin / {ADMIN_PASSWORD}, reader / {READER_PASSWORD})");
    Ok(())
}
