//! Milestone 3 integration test: the curation contract (§3.5, §3.6).
//!
//! The stage logic is unit-tested inside `src/curate/*`. What this file guards is
//! the contract *between* the curation stages and everything around them:
//!
//! * the recorded DeepSeek fixtures still parse through the real
//!   `score.rs` / `select.rs` / `editorial.rs` parsers into the structures the
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

use daily_epub::curate::editorial::FrontPageResponse;
use daily_epub::curate::profile;
use daily_epub::curate::score::parse_score_response;
use daily_epub::curate::select::parse_selection_response;
use daily_epub::types::LlmScore;

fn repo(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn fixture(name: &str) -> String {
    let path = repo(&format!("tests/fixtures/{name}"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Stage A responses must parse into `{id, score, category, rationale,
/// is_paywalled_guess}` per article (§3.6).
#[test]
fn stage_a_fixture_parses_into_scores() {
    let items = parse_score_response(&fixture("deepseek_score_batch.json"));
    assert!(items.len() >= 4, "fixture should cover a realistic batch");

    for item in &items {
        assert!(item.id > 0, "every item carries a positive article id");
        assert!(
            (0.0..=10.0).contains(&item.score),
            "score {} out of range",
            item.score
        );
        assert!(!item.category.is_empty());
        assert!(
            item.rationale.split_whitespace().count() <= 20,
            "rationale must stay under 20 words: {:?}",
            item.rationale
        );
    }
    assert!(
        items.iter().any(|i| i.is_paywalled_guess),
        "the fixture should exercise the paywall flag"
    );

    // The batch spans the rubric rather than clustering at one score.
    let scores: Vec<f64> = items.iter().map(|i| i.score).collect();
    let spread = scores.iter().cloned().fold(f64::MIN, f64::max)
        - scores.iter().cloned().fold(f64::MAX, f64::min);
    assert!(spread >= 3.0, "fixture scores are too uniform to be useful");

    // Every item converts into the shared curation type.
    let converted: Vec<LlmScore> = items.into_iter().map(LlmScore::from).collect();
    assert!(converted.iter().all(|s| (0.0..=10.0).contains(&s.score)));
}

/// The messy fixture must stay messy: it is what proves the parser is lenient
/// (string ids, string scores, out-of-range scores, junk entries).
#[test]
fn stage_a_messy_fixture_is_salvaged_not_rejected() {
    let raw = fixture("deepseek_score_batch_messy.json");
    // The hard cases are still present in the recording…
    assert!(raw.contains("\"id\": \""), "needs a string id");
    assert!(raw.contains("\"score\": \""), "needs a string score");

    // …and the real parser copes with all of them.
    let items = parse_score_response(&raw);
    assert!(!items.is_empty(), "the parser salvaged nothing");
    assert!(
        items.iter().all(|i| (0.0..=10.0).contains(&i.score)),
        "out-of-range scores must be clamped: {:?}",
        items.iter().map(|i| i.score).collect::<Vec<_>>()
    );
    assert!(items.iter().all(|i| i.id > 0), "id-less items are skipped");

    // A response that is not JSON at all degrades to "no scores", never a panic.
    assert!(parse_score_response("I'm sorry, I can't do that.").is_empty());
    assert!(parse_score_response("").is_empty());
}

/// Stage B responses must carry `{id, section, position, lead_story}` with
/// exactly one lead, and use only palette section names (§3.6).
#[test]
fn stage_b_fixture_parses_into_a_lineup() {
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

/// Stage C's front-page response must deserialize into a 250–400 word editor's
/// note plus per-section intros (§3.6).
#[test]
fn stage_c_fixture_parses_into_a_front_page() {
    let response: FrontPageResponse = serde_json::from_str(&fixture("deepseek_front_page.json"))
        .expect("the front-page fixture must match FrontPageResponse");

    let words = response.from_the_editor.split_whitespace().count();
    assert!(
        (150..=450).contains(&words),
        "From the Editor is {words} words; the prompt asks for 250-400"
    );
    assert!(
        response.from_the_editor.contains("\n\n"),
        "the prompt asks for 2-4 blank-line separated paragraphs"
    );
    assert!(
        !response.from_the_editor.contains("- "),
        "no bullet lists on the front page"
    );

    assert!(response.section_intros.len() >= 2);
    for (section, intro) in &response.section_intros {
        let words = intro.split_whitespace().count();
        assert!(
            (10..=90).contains(&words),
            "intro for {section} is {words} words; the prompt asks for 35-60"
        );
    }
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
    let document = profile::build(&interests, profile::NO_LEARNED_ADJUSTMENTS);
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
