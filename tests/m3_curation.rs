//! Milestone 3 integration test: the curation contract (§3.5, §3.6).
//!
//! The stage logic is unit-tested inside `src/curate/*`. What this file guards is
//! the contract *between* the curation stages and everything around them:
//!
//! * recorded DeepSeek fixtures still parse through the real
//!   `assess.rs` / `editor.rs` / `editorial.rs` parsers into the structures the
//!   pipeline consumes, and the lenient parsers still cope with the messy one;
//! * the shipped Scour OPML still yields the ~220 interests the taste profile is
//!   assembled from (§3.6a);
//! * the binary starts, migrates a fresh database and exposes the `--skip-llm`
//!   path that lets the pipeline run without a DeepSeek key (notes §6).
//!
//! No network, no API key, no DeepSeek call.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use daily_epub::curate::assess::parse_deep_response;
use daily_epub::curate::editor::parse_selection_response;
use daily_epub::curate::editorial::BriefResponse;
use daily_epub::curate::profile;

fn repo(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn fixture(name: &str) -> String {
    let path = repo(&format!("tests/fixtures/{name}"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Deep responses must parse into `{id, quality, fit, category, rationale,
/// paywalled_guess, facets}` per article (§12.1).
#[test]
fn deep_fixture_parses_into_assessments() {
    let sections = daily_epub::config::CurationConfig::default().sections;
    let items = parse_deep_response(&fixture("deepseek_deep_batch.json"), &sections);
    assert!(items.len() >= 4, "fixture should cover a realistic batch");

    for item in &items {
        assert!(item.id > 0, "every item carries a positive article id");
        assert!((0.0..=10.0).contains(&item.quality));
        assert!((0.0..=10.0).contains(&item.fit));
        assert!(item.category.is_some(), "every category is on the palette");
        assert!(
            item.rationale.split_whitespace().count() <= 25,
            "rationale must stay under 25 words: {:?}",
            item.rationale
        );
        assert!(item.facets.format.is_some() && item.facets.topic_group.is_some());
    }
    assert!(
        items.iter().any(|item| item.paywalled_guess),
        "the fixture should exercise the paywall flag"
    );
    // Quality and fit are judged separately: the fixture has an article whose
    // fit exceeds its quality and one the other way round.
    assert!(items.iter().any(|item| item.fit > item.quality));
    assert!(items.iter().any(|item| item.quality > item.fit));

    // The batch spans the rubric rather than clustering at one score.
    let qualities: Vec<f64> = items.iter().map(|i| i.quality).collect();
    let spread = qualities.iter().cloned().fold(f64::MIN, f64::max)
        - qualities.iter().cloned().fold(f64::MAX, f64::min);
    assert!(spread >= 3.0, "fixture scores are too uniform to be useful");
}

/// The messy fixture must stay messy: it is what proves the parser is lenient
/// (string ids and scores, out-of-range scores, unknown facet tokens, junk).
#[test]
fn deep_messy_fixture_is_salvaged_not_rejected() {
    let sections = daily_epub::config::CurationConfig::default().sections;
    let raw = fixture("deepseek_deep_batch_messy.json");
    assert!(raw.contains("\"id\": \""), "needs a string id");
    assert!(raw.contains("\"quality\": \""), "needs a string score");
    assert!(
        raw.contains("interactive_experience"),
        "needs an unknown facet token"
    );

    let items = parse_deep_response(&raw, &sections);
    assert!(!items.is_empty(), "the parser salvaged nothing");
    assert!(
        items
            .iter()
            .all(|i| (0.0..=10.0).contains(&i.quality) && (0.0..=10.0).contains(&i.fit)),
        "out-of-range scores must be clamped"
    );
    assert!(items.iter().all(|i| i.id > 0), "id-less items are skipped");
    let messy = items
        .iter()
        .find(|i| i.id == 202)
        .expect("string-valued item");
    assert!(
        messy.facets.format.is_none(),
        "unknown facet tokens become None"
    );
    assert_eq!(messy.facets.depth.as_deref(), Some("standard"));
    let off_palette = items.iter().find(|i| i.id == 204).expect("clamped item");
    assert!(
        off_palette.category.is_none(),
        "invented sections become None"
    );

    // A response that is not JSON at all degrades to "no assessments", never a panic.
    assert!(parse_deep_response("I'm sorry, I can't do that.", &sections).is_empty());
    assert!(parse_deep_response("", &sections).is_empty());
}

/// Editor responses must carry `{id, section, position, lead_story}` with
/// exactly one lead, and use only palette section names (plan §13).
#[test]
fn editor_fixture_parses_into_a_lineup() {
    // The palette from `CurationConfig::default()` (§3.14).
    let sections = daily_epub::config::CurationConfig::default().sections;

    let picks = parse_selection_response(&fixture("deepseek_lineup.json"));
    assert!(picks.len() >= 5);

    let mut ids = BTreeSet::new();
    let mut leads = 0;
    for pick in &picks {
        assert!(ids.insert(pick.id), "the same article was picked twice");
        assert!(
            sections.contains(&pick.section),
            "{:?} is not in the configured palette",
            pick.section
        );
        assert!(pick.position >= 1);
        if pick.lead_story {
            leads += 1;
            assert_eq!(
                pick.section, "Top Stories",
                "the lead sits in the first section"
            );
        }
    }
    assert_eq!(leads, 1, "exactly one lead story");
    // The reserved section is never offered to the model (§3.6, §3.8).
    assert!(
        !sections
            .iter()
            .any(|s| s == daily_epub::types::WORLD_BRIEFING_SECTION)
    );
}

/// The Brief must deserialize into 120–200 words of plain prose that names at
/// least three picks by title (§14.2). Section intros are gone.
#[test]
fn stage_c_fixture_parses_into_the_brief() {
    let response: BriefResponse = serde_json::from_str(&fixture("claude_brief.json"))
        .expect("the brief fixture must match BriefResponse");

    let words = response.brief.split_whitespace().count();
    assert!(
        (100..=220).contains(&words),
        "The Brief is {words} words; the prompt asks for 120-200"
    );
    assert!(
        !response.brief.contains("- ") && !response.brief.contains('#'),
        "no bullets or headings in the brief"
    );
    let titles = response.brief.matches('"').count() / 2;
    assert!(
        titles >= 3,
        "the brief names at least three picks; found {titles}"
    );
    for banned in [
        "delve",
        "dive",
        "explore",
        "a mix of",
        "something for everyone",
    ] {
        assert!(
            !response.brief.to_lowercase().contains(banned),
            "banned phrase {banned}"
        );
    }
    let value: serde_json::Value = serde_json::from_str(&fixture("claude_brief.json")).unwrap();
    assert!(value.get("section_intros").is_none());
}

/// The taste profile is seeded from this file; a broken export would silently
/// gut the system prompt (§3.6a).
#[test]
fn scour_opml_still_yields_the_interest_list() {
    let interests = profile::parse_interests(&repo("data/scour-interests.opml"))
        .expect("the shipped OPML must parse");
    let unique: BTreeSet<String> = interests.iter().map(|n| n.to_lowercase()).collect();

    assert!(
        unique.len() > 180,
        "expected ~220 interests, found {}",
        unique.len()
    );
    for expected in [
        "rust",
        "boston tech",
        "e-ink displays",
        "self-hosted",
        "sci-fi",
    ] {
        assert!(unique.contains(expected), "{expected} disappeared");
    }
    assert!(
        !interests.iter().any(|n| n.contains("token=")),
        "interest names must not leak the Scour token"
    );

    // The assembled profile is what actually reaches DeepSeek as the system
    // prompt; it must mention the stated preferences and the interests (§3.6).
    let profile_file = profile::load_profile(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/profile.md"
    )))
    .expect("profile file");
    let document = profile::build(
        &profile_file.body,
        &interests,
        profile::NO_LEARNED_ADJUSTMENTS,
        &[],
        60,
    );
    assert!(document.contains("Rust"));
    assert!(document.len() > 1000, "the profile is suspiciously short");
}

/// The binary must migrate a fresh database and advertise the offline
/// `--skip-llm` path (§2, notes §6).
#[test]
fn binary_migrates_and_offers_the_skip_llm_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("nested").join("daily-epub.db");
    let bin = env!("CARGO_BIN_EXE_daily-epub");

    let out = Command::new(bin)
        .args(["db", "migrate"])
        .env("DAILY_EPUB_DATABASE_PATH", &db_path)
        .output()
        .expect("running `daily-epub db migrate`");
    assert!(
        out.status.success(),
        "db migrate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db_path.exists(), "the database was not created");

    // Re-running is idempotent.
    let out = Command::new(bin)
        .args(["db", "migrate"])
        .env("DAILY_EPUB_DATABASE_PATH", &db_path)
        .output()
        .expect("re-running `daily-epub db migrate`");
    assert!(out.status.success());

    let out = Command::new(bin)
        .args(["generate", "--help"])
        .output()
        .expect("running `daily-epub generate --help`");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        help.contains("--skip-llm"),
        "generate must expose --skip-llm"
    );
    assert!(help.contains("--dry-run"));
    assert!(help.contains("--max-articles"));
}

/// An unreachable Miniflux must fail the run cleanly, with the failure recorded
/// and an error chain a human can act on (§5 verification).
#[test]
fn generate_fails_cleanly_when_miniflux_is_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_daily-epub");

    let out = Command::new(bin)
        .args([
            "generate",
            "--dry-run",
            "--skip-llm",
            "--date",
            "2026-08-15",
        ])
        .env("DAILY_EPUB_DATABASE_PATH", dir.path().join("daily-epub.db"))
        .env("DAILY_EPUB_OUT_DIR", dir.path().join("out"))
        // Port 1 is reserved and never listening.
        .env("DAILY_EPUB_MINIFLUX__BASE_URL", "http://127.0.0.1:1")
        .env("DAILY_EPUB_MINIFLUX__API_KEY", "not-a-real-key")
        .output()
        .expect("running `daily-epub generate`");

    assert!(!out.status.success(), "the run must not report success");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ingesting entries from miniflux"),
        "the error chain must name the failing stage:\n{stderr}"
    );
    assert!(
        stderr.contains("miniflux") && stderr.contains("127.0.0.1:1"),
        "the error chain must name the unreachable endpoint:\n{stderr}"
    );
    // Nothing was written to the output directory.
    assert!(
        !dir.path()
            .join("out")
            .join("The Daily EPUB - 2026-08-15.epub")
            .exists()
    );
}
