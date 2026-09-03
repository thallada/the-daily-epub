//! The capstone test: the whole pipeline, driven as a library, with no network
//! (spec §2, §5).
//!
//! ```text
//! synthetic entries → dedupe → extract (offline) → persist → admission
//!   → select → editorial → issue → both EPUB editions → publish → OPDS + rows
//! ```
//!
//! Two passes over the same machinery:
//!
//! * [`skip_llm_pipeline_produces_a_published_issue`] takes the `--skip-llm`
//!   route (preliminary blend selects, feed excerpts stand in for summaries);
//! * [`llm_pipeline_runs_against_a_mock_backend`] takes the DeepSeek route with
//!   [`MockBackend`] standing in for the API, so stages A, B and C are all
//!   exercised — prompts, parsers, budget accounting and all — offline.
//!
//! `Extractor::offline` guarantees the extraction stage never opens a socket, and
//! no article in the fixtures carries an image, so the EPUB builder's image
//! downloader has nothing to fetch.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use jiff::Timestamp;
use jiff::civil::Date;

use daily_epub::config::{Config, PublishConfig, ServerConfig, XtcConfig};
use daily_epub::curate::llm::{LlmClient, Llms, MockBackend, UsageMeter};
use daily_epub::curate::{Curator, admit, editorial, rank};
use daily_epub::db::Db;
use daily_epub::extract::Extractor;
use daily_epub::types::{
    Article, Candidate, Colophon, Edition, Entry, Issue, Lineup, Models, SourceKind, Vote,
};
use daily_epub::{auth, dedupe, epub, miniflux, pipeline, publish};

const SECRET: &str = "e2e-secret";

fn ts(s: &str) -> Timestamp {
    s.parse().expect("timestamp")
}

fn date() -> Date {
    "2026-08-15".parse().expect("date")
}

fn body(words: usize) -> String {
    format!(
        "<p>{}</p>",
        "a sentence about database internals and page layout ".repeat(words / 8)
    )
}

/// A config whose every writable path points inside `root`.
fn test_config(root: &Path) -> Config {
    Config {
        database_path: root.join("db").join("daily-epub.db"),
        out_dir: root.join("out"),
        target_article_count: 6,
        world_briefing: false,
        publish: PublishConfig {
            epub_dir: root.join("bookorbit"),
            xtc_dir: root.join("xtc"),
        },
        xtc: XtcConfig {
            // Never shell out to node in a test.
            enabled: false,
            ..XtcConfig::default()
        },
        server: ServerConfig {
            public_url: "https://daily.hallada.net".into(),
            hmac_secret: Some(SECRET.into()),
            ..ServerConfig::default()
        },
        ..Config::default()
    }
}

/// One day of ingest: eight entries covering duplicates, an excerpt-only story,
/// and three things that are not articles at all.
fn ingested() -> Vec<Entry> {
    let base = Entry {
        id: 0,
        feed_id: 0,
        feed_title: None,
        category: Some("Tech".into()),
        title: String::new(),
        url: String::new(),
        canonical_url: None,
        author: Some("Dana Author".into()),
        published_at: Some(ts("2026-08-15T04:00:00Z")),
        comments_url: None,
        raw_content: String::new(),
        fetched_at: ts("2026-08-15T05:30:00Z"),
    };

    vec![
        Entry {
            id: 101,
            feed_id: 1,
            feed_title: Some("Hacker News Front Page".into()),
            title: "A Deep Dive Into B-Trees".into(),
            url: "https://blog.dev/b-trees?utm_source=hnrss".into(),
            comments_url: Some("https://news.ycombinator.com/item?id=41234567".into()),
            raw_content: "<p>Discussion link only.</p>".into(),
            ..base.clone()
        },
        // Same story, richer body: the cluster keeps this one.
        Entry {
            id: 102,
            feed_id: 2,
            feed_title: Some("Scour: Databases".into()),
            title: "A Deep Dive Into B-Trees".into(),
            url: "https://blog.dev/b-trees#intro".into(),
            raw_content: body(900),
            ..base.clone()
        },
        Entry {
            id: 103,
            feed_id: 3,
            feed_title: Some("The Rust Blog".into()),
            title: "Async Cancellation, Revisited".into(),
            url: "https://rust.dev/cancellation".into(),
            raw_content: body(1400),
            ..base.clone()
        },
        Entry {
            id: 104,
            feed_id: 4,
            feed_title: Some("Astronomy Notes".into()),
            title: "What Webb Saw in the Rings of Uranus".into(),
            url: "https://space.dev/webb-uranus".into(),
            raw_content: body(700),
            ..base.clone()
        },
        Entry {
            id: 105,
            feed_id: 5,
            feed_title: Some("Boston Civic Tech".into()),
            title: "The MBTA's New Signal Priority Pilot".into(),
            url: "https://boston.dev/signal-priority".into(),
            raw_content: body(600),
            ..base.clone()
        },
        // Excerpt only: penalized by the pre-filter but still eligible (§3.5).
        Entry {
            id: 106,
            feed_id: 6,
            feed_title: Some("Lobsters".into()),
            title: "Notes on Writing a Toy Allocator".into(),
            url: "https://other.dev/allocator".into(),
            comments_url: Some("https://lobste.rs/s/abcdef/notes".into()),
            raw_content: "<p>A teaser paragraph and nothing else.</p>".into(),
            ..base.clone()
        },
        // Non-articles: a video host and an empty title (§3.2).
        Entry {
            id: 107,
            feed_id: 7,
            feed_title: Some("Video Feed".into()),
            title: "A conference talk".into(),
            url: "https://www.youtube.com/watch?v=abc".into(),
            raw_content: "<p>watch it</p>".into(),
            ..base.clone()
        },
        Entry {
            id: 108,
            feed_id: 8,
            feed_title: Some("Broken Feed".into()),
            title: "   ".into(),
            url: "https://broken.dev/x".into(),
            raw_content: body(400),
            ..base.clone()
        },
    ]
}

/// Stages 1–4: ingest → dedupe → extract → persist, exactly as `pipeline.rs`
/// orders them (extraction runs *before* the article rows are written).
async fn ingest_dedupe_extract_persist(db: &Db) -> Vec<Article> {
    let entries = ingested();
    db.upsert_entries(&entries).await.expect("persist entries");

    let feeds = std::collections::HashMap::from([(
        2,
        miniflux::FeedMeta {
            id: 2,
            title: "Scour: Databases".into(),
            site_url: "https://scour.ing".into(),
            feed_url: "https://scour.ing/feed?interest=databases".into(),
            category: Some("Interests".into()),
        },
    )]);
    let (mut articles, stats) = dedupe::cluster_with_feeds(entries, &miniflux::feed_urls(&feeds));
    assert_eq!(stats.entries_in, 8);
    assert_eq!(stats.dropped_non_article, 2, "video host + empty title");
    assert_eq!(stats.merged, 1, "the B-trees story arrived twice");
    assert_eq!(articles.len(), 5);

    let extractor = Extractor::offline(vec![]);
    assert!(
        !extractor.can_fetch(),
        "the test must never hit the network"
    );
    let extracted = extractor.extract_all(&mut articles).await;
    assert_eq!(extracted.excerpt_only, 1, "the allocator teaser");

    for article in &mut articles {
        article.id = db.upsert_article(article).await.expect("persist article");
        assert!(article.id > 0);
    }
    let b_trees = articles
        .iter()
        .find(|a| a.canonical_url == "https://blog.dev/b-trees")
        .expect("the merged cluster");
    assert!(b_trees.came_via(SourceKind::Scour));
    assert!(b_trees.came_via(SourceKind::HnFrontpage));
    assert_eq!(b_trees.best_entry_id, 102, "the richest body won");

    articles
}

/// Stages 11–14: assemble, build both editions, publish, record.
async fn assemble_build_publish(
    db: &Db,
    cfg: &Config,
    lineup: Lineup,
    colophon: Colophon,
) -> Issue {
    let editorial_doc = editorial::fallback_editorial(&lineup);
    let mut lineup = lineup;
    pipeline::apply_summaries(&mut lineup, &editorial_doc);
    assert!(
        lineup.picks.iter().all(|p| p.summary.is_some()),
        "every pick carries a summary before the EPUB is built"
    );

    let issue_number = db.next_issue_number(date()).await.expect("issue number");
    let issue = pipeline::build_issue(
        date(),
        issue_number,
        ts("2026-08-15T09:30:00Z"),
        lineup,
        editorial_doc,
        None,
        colophon,
    );
    assert_eq!(issue.meta.display_date, "Saturday, August 15, 2026");

    // --- EPUB: both editions (§3.10) ---
    let (artifacts, images) = epub::build_all(&issue, cfg, &cfg.out_dir)
        .await
        .expect("both editions build");
    assert_eq!(artifacts.len(), 2);
    assert_eq!(images, 0, "the fixtures carry no images");
    for artifact in &artifacts {
        assert!(artifact.path.exists(), "{}", artifact.path.display());
        assert!(artifact.bytes > 1000);
        let zip = std::fs::read(&artifact.path).expect("read epub");
        assert_eq!(&zip[0..4], b"PK\x03\x04", "is a zip");
        assert_eq!(&zip[38..58], b"application/epub+zip");
    }
    assert!(
        cfg.out_dir
            .join("The Daily EPUB - 2026-08-15.epub")
            .exists()
    );
    assert!(
        cfg.out_dir
            .join("The Daily EPUB - 2026-08-15 (X4).epub")
            .exists()
    );

    // Rating links are signed with the configured secret and are what the
    // running server verifies (§3.9). The chapters are deflated inside the zip,
    // so assert on the rendered XHTML the builder just zipped.
    let chapters = epub::build::render_all(
        &issue,
        Edition::Standard,
        &[],
        &cfg.server.public_url,
        cfg.server.hmac_secret.as_deref(),
    )
    .expect("render chapters");
    let first = issue.lineup.picks[0].article.id;
    let expected = auth::rating_url(&cfg.server.public_url, SECRET, date(), first, Vote::Loved);
    let chapter = chapters
        .iter()
        .find(|c| c.id == format!("art-{}", issue.lineup.picks[0].article.best_entry_id))
        .expect("the first article has a chapter");
    assert!(
        chapter.xhtml.contains(&expected),
        "the article footer must carry {expected}\n{}",
        chapter.xhtml
    );
    assert!(
        daily_epub::server::verify_token(
            SECRET,
            date(),
            first,
            Vote::Loved,
            &auth::rating_token(SECRET, date(), first, Vote::Loved)
        ),
        "the server must accept the token the EPUB minted"
    );

    // --- Publish (§3.11) ---
    let published = publish::publish_issue(cfg, &issue, &artifacts, None)
        .await
        .expect("publish");
    assert_eq!(published.epubs.len(), 2);
    for artifact in &published.epubs {
        assert!(artifact.path.starts_with(&cfg.publish.epub_dir));
        assert!(artifact.path.exists(), "{}", artifact.path.display());
    }
    assert!(
        cfg.publish
            .epub_dir
            .join("The Daily EPUB - 2026-08-15.epub")
            .exists()
    );
    assert!(
        cfg.publish
            .epub_dir
            .join("The Daily EPUB - 2026-08-15 (X4).epub")
            .exists()
    );
    assert!(published.xtc.is_none(), "the converter is disabled here");

    // The OPDS feed is derived from what was just published — both editions,
    // typed so CrossPoint will accept them (§3.11).
    let feed = publish::build_opds(db, cfg).await.expect("build the feed");
    assert!(feed.starts_with("<?xml"), "{feed}");
    assert!(feed.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\""));
    assert!(feed.contains(&cfg.server.public_url));
    assert_eq!(feed.matches("<entry>").count(), 2, "{feed}");
    assert_eq!(feed.matches("application/epub+zip").count(), 2, "{feed}");
    assert!(
        feed.contains("The Daily EPUB — 2026-08-15</title>"),
        "{feed}"
    );
    assert!(
        feed.contains("The Daily EPUB — 2026-08-15 (X4)</title>"),
        "{feed}"
    );

    // --- Record (§3.13) ---
    let epub_path = published
        .epubs
        .iter()
        .find(|a| a.edition == Edition::Standard)
        .map(|a| a.path.display().to_string());
    let x4_path = published
        .epubs
        .iter()
        .find(|a| a.edition == Edition::X4)
        .map(|a| a.path.display().to_string());
    db.upsert_issue(
        date(),
        issue.meta.issue_number,
        issue.meta.generated_at,
        epub_path.as_deref(),
        x4_path.as_deref(),
        None,
        Some(&issue.editorial.front_page_html),
        Some("{\"status\":\"ok\"}"),
        None,
    )
    .await
    .expect("record the issue");
    db.replace_issue_articles(date(), &issue.lineup.picks)
        .await
        .expect("record the lineup");

    let reports = db.recent_reports(5).await.expect("recent reports");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].0, date());
    assert_eq!(reports[0].1.as_deref(), Some("{\"status\":\"ok\"}"));

    let published_ids = db
        .previously_published_ids()
        .await
        .expect("issue_articles rows");
    assert_eq!(published_ids.len(), issue.lineup.picks.len());

    // Tomorrow's issue is No. 2 (days since the first issue, §3.10).
    let tomorrow: Date = "2026-08-16".parse().unwrap();
    assert_eq!(db.next_issue_number(tomorrow).await.unwrap(), 2);

    issue
}

#[tokio::test]
async fn skip_llm_pipeline_produces_a_published_issue() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(root.path());
    let db = Db::open_and_migrate(&cfg.database_path)
        .await
        .expect("open db");

    let articles = ingest_dedupe_extract_persist(&db).await;

    // --- Stages 6–7 with no LLM at all (notes §6) ---
    let curator = Curator::new(cfg.clone(), db.clone(), Llms::default());
    let mut personalized = articles
        .into_iter()
        .map(|article| Candidate::new(article, false))
        .collect::<Vec<_>>();
    for candidate in &mut personalized {
        candidate.signals.preliminary = candidate.signals.heuristic;
    }
    admit::admit(&mut personalized, date(), &cfg.curation.ranking);
    let candidates = personalized
        .into_iter()
        .filter(|candidate| candidate.stage == "admitted")
        .collect::<Vec<_>>();
    assert_eq!(candidates.len(), 5, "nothing is dropped at this volume");
    // The excerpt-only story is penalized (§3.5).
    let allocator = candidates
        .iter()
        .find(|c| c.article.excerpt_only)
        .expect("the allocator teaser survived");
    assert!(
        allocator.signals.heuristic.unwrap_or_default()
            < candidates
                .iter()
                .filter_map(|candidate| candidate.signals.heuristic)
                .fold(f64::NEG_INFINITY, f64::max)
    );

    let lineup = curator.select(candidates, date()).await.expect("select");
    assert_eq!(lineup.picks.len(), 5, "target 6, only 5 candidates exist");
    assert!(lineup.lead().is_some(), "a lead story is always chosen");
    assert!(!lineup.section_order.is_empty());
    assert!(
        lineup
            .picks
            .iter()
            .all(|p| lineup.section_order.contains(&p.section)),
        "every pick sits in a listed section"
    );

    let colophon = Colophon {
        provider_costs: BTreeMap::new(),
        models: Models {
            bulk: "none".into(),
            editor: "none".into(),
            summaries: "none".into(),
        },
        entries_fetched: 8,
        feeds_seen: 8,
        candidates: 5,
        cost_usd: 0.0,
        generator_version: format!("daily-epub {}", daily_epub::VERSION),
    };
    let issue = assemble_build_publish(&db, &cfg, lineup, colophon).await;

    // Front page and summaries came from excerpts, not from a model.
    assert!(!issue.editorial.front_page_html.is_empty());
    assert_eq!(issue.editorial.summaries.len(), issue.lineup.picks.len());

    // Re-running the same date replaces rather than duplicates (notes §12).
    let republished = publish::publish_issue(
        &cfg,
        &issue,
        &[
            daily_epub::types::Artifact {
                edition: Edition::Standard,
                path: cfg.out_dir.join("The Daily EPUB - 2026-08-15.epub"),
                bytes: 0,
            },
            daily_epub::types::Artifact {
                edition: Edition::X4,
                path: cfg.out_dir.join("The Daily EPUB - 2026-08-15 (X4).epub"),
                bytes: 0,
            },
        ],
        None,
    )
    .await
    .expect("republish");
    assert_eq!(republished.epubs.len(), 2);
    let files: Vec<String> = std::fs::read_dir(&cfg.publish.epub_dir)
        .expect("read bookorbit dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(files.len(), 2, "no duplicate files: {files:?}");

    db.replace_issue_articles(date(), &issue.lineup.picks)
        .await
        .expect("replace lineup");
    assert_eq!(
        db.previously_published_ids().await.unwrap().len(),
        issue.lineup.picks.len(),
        "issue_articles was replaced, not appended"
    );
}

#[tokio::test]
async fn llm_pipeline_runs_against_a_mock_backend() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(root.path());
    let db = Db::open_and_migrate(&cfg.database_path)
        .await
        .expect("open db");

    let articles = ingest_dedupe_extract_persist(&db).await;
    let mut personalized = articles
        .into_iter()
        .map(|article| Candidate::new(article, false))
        .collect::<Vec<_>>();
    for candidate in &mut personalized {
        candidate.signals.preliminary = candidate.signals.heuristic;
    }
    admit::admit(&mut personalized, date(), &cfg.curation.ranking);
    let candidates = personalized
        .into_iter()
        .filter(|candidate| candidate.stage == "admitted")
        .collect::<Vec<_>>();
    let ids: Vec<i64> = candidates.iter().map(|c| c.article.id).collect();
    assert_eq!(ids.len(), 5);

    // --- Script DeepSeek: one deep-assessment batch, one editor call, five
    // summaries and one brief. ---
    let backend = std::sync::Arc::new(MockBackend::new());
    let usage = daily_epub::types::TokenUsage {
        input_tokens: 1000,
        cached_tokens: 500,
        cache_write_tokens: 0,
        output_tokens: 200,
    };
    let scores: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": {id}, "quality": {}, "fit": 7, "category": "Tech & Engineering",
                     "rationale": "solid systems writeup", "paywalled_guess": false, "facets": {{"format":"analysis_essay"}}}}"#,
                9 - i
            )
        })
        .collect();
    backend.push(format!("{{\"articles\": [{}]}}", scores.join(",")), usage);

    let picks: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": {id}, "section": "{}", "position": {}, "lead_story": {}}}"#,
                if i == 0 {
                    "Top Stories"
                } else {
                    "Tech & Engineering"
                },
                i + 1,
                i == 0
            )
        })
        .collect();
    backend.push(format!("{{\"picks\": [{}]}}", picks.join(",")), usage);

    for id in &ids {
        backend.push(
            format!(r#"{{"summary": "A newspaper abstract for article {id}."}}"#),
            usage,
        );
    }
    backend.push(
        r#"{"brief": "Today's issue leans on storage internals.\n\nRead on."}"#,
        usage,
    );

    let meter = UsageMeter::for_provider(&cfg.providers["deepseek"]);
    let llm = LlmClient::with_backend(
        &cfg.providers["deepseek"].model,
        "You are the editor of The Daily EPUB.".into(),
        meter.clone(),
        backend.clone(),
    );
    let curator = Curator::new(
        cfg.clone(),
        db.clone(),
        Llms {
            bulk: Some(llm),
            editor: None,
        },
    );

    let mut candidates = candidates;
    curator
        .assess(&mut candidates, true, None, jiff::Timestamp::now())
        .await
        .expect("deep assessment");
    assert!(
        candidates.iter().all(|c| c.assessment.deep.is_some()),
        "every candidate came back assessed"
    );

    let lineup = curator.select(candidates, date()).await.expect("editor");
    assert_eq!(lineup.picks.len(), 5);
    assert_eq!(
        lineup.lead().map(|p| p.article.id),
        Some(ids[0]),
        "the model's lead choice is honored"
    );
    assert_eq!(
        lineup.section_order.first().map(String::as_str),
        Some("Top Stories")
    );

    let editorial_doc = curator.editorial(&lineup).await.expect("stage C");
    assert_eq!(editorial_doc.summaries.len(), 5);
    assert!(
        editorial_doc
            .summaries
            .values()
            .all(|s| s.contains("newspaper abstract")),
        "the model's summaries were used, not excerpts"
    );
    assert!(editorial_doc.front_page_html.contains("storage internals"));
    assert!(
        lineup.picks.iter().all(|p| p.why.is_none()),
        "the scripted editor gave no why lines"
    );

    // Every scripted response was consumed, and the meter priced them (§3.6).
    assert_eq!(backend.calls(), 1 + 1 + 5 + 1);
    let total = meter.total();
    assert_eq!(total.input_tokens, 8 * usage.input_tokens);
    assert!(meter.cost_usd() > 0.0 && !meter.budget_exceeded());
    // The taste profile leads every request, byte for byte — that is what makes
    // DeepSeek's prefix cache hit (§3.6).
    let prompts = backend.prompts();
    assert!(
        prompts
            .iter()
            .all(|p| p.system.starts_with("You are the editor")),
        "the system prompt must be identical across requests"
    );

    // And it all assembles, builds and publishes like the skip-llm route does.
    let colophon = Colophon {
        provider_costs: BTreeMap::from([("deepseek".to_string(), meter.cost_usd())]),
        models: Models {
            bulk: cfg.providers["deepseek"].model.clone(),
            editor: format!("{} (bulk fallback)", cfg.providers["deepseek"].model),
            summaries: cfg.providers["deepseek"].model.clone(),
        },
        entries_fetched: 8,
        feeds_seen: 8,
        candidates: 5,
        cost_usd: meter.cost_usd(),
        generator_version: format!("daily-epub {}", daily_epub::VERSION),
    };
    let mut lineup = lineup;
    pipeline::apply_summaries(&mut lineup, &editorial_doc);
    let issue = assemble_build_publish(&db, &cfg, lineup, colophon).await;
    assert_eq!(issue.colophon.models.bulk, cfg.providers["deepseek"].model);
    assert!(issue.colophon.cost_usd > 0.0);
}

#[tokio::test]
async fn failing_deepseek_still_publishes_with_heuristic_fallbacks() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(root.path());
    let db = Db::open_and_migrate(&cfg.database_path)
        .await
        .expect("open db");
    let articles = ingest_dedupe_extract_persist(&db).await;
    let mut personalized = articles
        .into_iter()
        .map(|article| Candidate::new(article, false))
        .collect::<Vec<_>>();
    for candidate in &mut personalized {
        candidate.signals.preliminary = candidate.signals.heuristic;
    }
    admit::admit(&mut personalized, date(), &cfg.curation.ranking);
    let mut candidates = personalized
        .into_iter()
        .filter(|candidate| candidate.stage == "admitted")
        .collect::<Vec<_>>();

    let backend = std::sync::Arc::new(MockBackend::new());
    let client = LlmClient::with_backend(
        &cfg.providers["deepseek"].model,
        "reader profile".into(),
        UsageMeter::for_provider(&cfg.providers["deepseek"]),
        backend.clone(),
    );
    let curator = Curator::new(
        cfg.clone(),
        db.clone(),
        Llms {
            bulk: Some(client),
            editor: None,
        },
    );
    curator
        .assess(&mut candidates, true, None, jiff::Timestamp::now())
        .await
        .expect("failed batches degrade, not abort");
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.assessment.deep.is_none())
    );
    // Utility over the present signals, clustered as singletons without
    // embeddings: every admitted article is shortlisted with a cluster id.
    let ranked = rank::shortlist(&mut candidates, &HashMap::new(), &cfg.curation.ranking);
    assert_eq!(ranked.shortlisted, candidates.len());
    assert_eq!(ranked.clusters, candidates.len());
    for candidate in &candidates {
        assert_eq!(candidate.stage, "shortlisted");
        assert!(candidate.utility.is_some() && candidate.cluster.is_some());
        assert!(candidate.rank_utility.is_some());
    }
    let mut lineup = curator
        .select(candidates, date())
        .await
        .expect("fallback lineup");
    let editorial = curator
        .editorial(&lineup)
        .await
        .expect("fallback editorial");
    pipeline::apply_summaries(&mut lineup, &editorial);
    assert!(!lineup.picks.is_empty());
    assert!(backend.calls() > 0, "the failing backend was exercised");
    let issue = assemble_build_publish(
        &db,
        &cfg,
        lineup,
        Colophon {
            provider_costs: BTreeMap::new(),
            models: Models {
                bulk: cfg.providers["deepseek"].model.clone(),
                editor: format!("{} (bulk fallback)", cfg.providers["deepseek"].model),
                summaries: cfg.providers["deepseek"].model.clone(),
            },
            entries_fetched: 8,
            feeds_seen: 8,
            candidates: 5,
            cost_usd: 0.0,
            generator_version: format!("daily-epub {}", daily_epub::VERSION),
        },
    )
    .await;
    assert!(!issue.lineup.picks.is_empty());
    assert_eq!(std::fs::read_dir(&cfg.publish.epub_dir).unwrap().count(), 2);
}
