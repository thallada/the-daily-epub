//! M4 — a complete issue EPUB, built offline (spec §3.10, §4 M4).
//!
//! Everything here is offline: the synthetic issue's images are never
//! downloaded, so the chapters exercise the placeholder path.

use daily_epub::config::{self, Config};
use daily_epub::epub::build::fixtures;
use daily_epub::epub::{self, build};
use daily_epub::types::{Edition, Issue, Vote};
use daily_epub::{comments, world};

/// Local file headers store entry names verbatim, so a byte search over the
/// archive is enough to assert its contents without a zip reader.
fn contains_entry(zip: &[u8], name: &str) -> bool {
    zip.windows(name.len()).any(|w| w == name.as_bytes())
}

/// Read one entry out of the archive, inflating it.
fn read_entry_bytes(zip: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read as _;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("zip opens");
    let mut file = archive.by_name(name).expect("entry exists");
    let mut out = Vec::new();
    file.read_to_end(&mut out).expect("entry reads");
    out
}

fn read_entry(zip: &[u8], name: &str) -> String {
    use std::io::Read as _;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("zip opens");
    let mut file = archive.by_name(name).expect("entry exists");
    let mut out = String::new();
    file.read_to_string(&mut out).expect("entry is text");
    out
}

/// Builds one edition into a temporary directory; the directory is cleaned up
/// when the returned `TempDir` is dropped.
fn build_edition_to_bytes(
    issue: &Issue,
    edition: Edition,
) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
    let cfg = Config {
        server: config::ServerConfig {
            public_url: "https://daily.hallada.net".into(),
            hmac_secret: Some("integration-secret".into()),
            ..config::ServerConfig::default()
        },
        ..Config::default()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = epub::build_edition_with_images(issue, edition, &cfg, dir.path(), &[])
        .expect("edition builds");
    let bytes = std::fs::read(&artifact.path).expect("read epub");
    assert_eq!(artifact.bytes as usize, bytes.len());
    (dir, artifact.path, bytes)
}

#[test]
fn standard_edition_is_a_well_formed_epub3_archive() {
    let issue = fixtures::issue();
    let (_dir, path, zip) = build_edition_to_bytes(&issue, Edition::Standard);

    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("The Daily EPUB - 2026-08-15.epub")
    );
    assert_eq!(&zip[0..4], b"PK\x03\x04", "starts with a zip local header");
    assert_eq!(
        &zip[30..38],
        b"mimetype",
        "`mimetype` must be the first entry"
    );
    assert_eq!(&zip[38..58], b"application/epub+zip");

    for entry in [
        "META-INF/container.xml",
        "OEBPS/content.opf",
        "OEBPS/toc.ncx",
        "OEBPS/nav.xhtml",
        "OEBPS/stylesheet.css",
        "OEBPS/cover.png",
        "OEBPS/cover.xhtml",
        "OEBPS/front.xhtml",
        "OEBPS/in-this-issue.xhtml",
        "OEBPS/sec-top-stories.xhtml",
        "OEBPS/art-1001.xhtml",
        "OEBPS/disc-1001.xhtml",
        "OEBPS/sec-niche-corner.xhtml",
        "OEBPS/art-1002.xhtml",
        "OEBPS/world.xhtml",
        "OEBPS/colophon.xhtml",
    ] {
        assert!(contains_entry(&zip, entry), "missing {entry}");
    }
}

#[test]
fn x4_edition_is_built_alongside_the_standard_one() {
    let issue = fixtures::issue();
    let (_dir, path, zip) = build_edition_to_bytes(&issue, Edition::X4);
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("The Daily EPUB - 2026-08-15 (X4).epub")
    );
    assert_eq!(&zip[30..38], b"mimetype");
    assert!(contains_entry(&zip, "OEBPS/art-1001.xhtml"));
    assert!(contains_entry(&zip, "OEBPS/cover.jpg"));
    assert!(!contains_entry(&zip, "OEBPS/cover.png"));
    let jpeg = read_entry_bytes(&zip, "OEBPS/cover.jpg");
    let decoded = image::load_from_memory(&jpeg).expect("X4 cover decodes");
    assert_eq!((decoded.width(), decoded.height()), (480, 800));
    assert_eq!(decoded.color(), image::ColorType::Rgb8);
    assert!(jpeg.windows(2).any(|marker| marker == [0xff, 0xc0]));
    assert!(!jpeg.windows(2).any(|marker| marker == [0xff, 0xc2]));
    let opf = read_entry(&zip, "OEBPS/content.opf");
    let cover_page = read_entry(&zip, "OEBPS/cover.xhtml");
    assert!(opf.contains("href=\"cover.jpg\""), "{opf}");
    assert!(opf.contains("media-type=\"image/jpeg\""), "{opf}");
    assert!(opf.contains("properties=\"cover-image\""), "{opf}");
    assert!(
        opf.contains("<meta name=\"cover\" content=\"cover-image\"/>"),
        "{opf}"
    );
    assert!(cover_page.contains("src=\"cover.jpg\""), "{cover_page}");
}

/// Both editions land in the same BookOrbit library, which lists books by
/// `dc:title` — so the edition has to be in the title, not just the filename
/// (§3.10). Without this the two are indistinguishable in the library UI and
/// over OPDS.
#[test]
fn the_two_editions_have_distinct_titles_in_the_opf() {
    let issue = fixtures::issue();
    let (_d1, _, standard) = build_edition_to_bytes(&issue, Edition::Standard);
    let (_d2, _, x4) = build_edition_to_bytes(&issue, Edition::X4);

    let standard_opf = read_entry(&standard, "OEBPS/content.opf");
    let x4_opf = read_entry(&x4, "OEBPS/content.opf");

    assert!(
        standard_opf.contains("<dc:title>The Daily EPUB \u{2014} 2026-08-15</dc:title>"),
        "{standard_opf}"
    );
    assert!(
        x4_opf.contains("<dc:title>The Daily EPUB \u{2014} 2026-08-15 (X4)</dc:title>"),
        "{x4_opf}"
    );

    // The series metadata still groups them: same collection, same position, so
    // they sort together rather than as two unrelated books.
    for opf in [&standard_opf, &x4_opf] {
        assert!(opf.contains("belongs-to-collection"), "{opf}");
        assert!(opf.contains("<dc:date>2026-08-15</dc:date>"), "{opf}");
        assert!(opf.contains("<dc:language>en</dc:language>"), "{opf}");
        assert_eq!(opf.matches("id=\"epub-creator-0\"").count(), 1, "{opf}");
    }
}

#[test]
fn chapter_ids_hrefs_and_toc_levels_are_stable() {
    let issue = fixtures::issue();
    let first =
        build::render_all(&issue, Edition::Standard, &[], "https://x.test", None).expect("render");
    let second =
        build::render_all(&issue, Edition::Standard, &[], "https://x.test", None).expect("render");
    assert_eq!(first, second, "rendering is deterministic");

    let map: Vec<(String, String, u8)> = first
        .iter()
        .map(|c| (c.id.clone(), c.href.clone(), c.toc_level))
        .collect();
    assert_eq!(
        map,
        vec![
            ("cover".into(), "cover.xhtml".into(), 1),
            ("front".into(), "front.xhtml".into(), 1),
            ("in-this-issue".into(), "in-this-issue.xhtml".into(), 1),
            ("sec-Top Stories".into(), "sec-top-stories.xhtml".into(), 1),
            ("art-1001".into(), "art-1001.xhtml".into(), 2),
            ("disc-1001".into(), "disc-1001.xhtml".into(), 3),
            (
                "sec-Niche Corner".into(),
                "sec-niche-corner.xhtml".into(),
                1
            ),
            ("art-1002".into(), "art-1002.xhtml".into(), 2),
            ("world".into(), "world.xhtml".into(), 1),
            ("colophon".into(), "colophon.xhtml".into(), 1),
        ]
    );
}

#[test]
fn every_chapter_is_parseable_xhtml() {
    let issue = fixtures::issue();
    let chapters = build::render_all(
        &issue,
        Edition::Standard,
        &[],
        "https://daily.hallada.net",
        Some("integration-secret"),
    )
    .expect("render");

    for chapter in &chapters {
        let xhtml = &chapter.xhtml;
        assert!(
            xhtml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"),
            "{} lacks an XML prologue",
            chapter.id
        );
        assert!(xhtml.contains("xmlns=\"http://www.w3.org/1999/xhtml\""));
        assert!(xhtml.trim_end().ends_with("</html>"));
        // Undefined XML entities (html5ever's `&nbsp;`) would break XML parsers.
        assert!(!xhtml.contains("&nbsp;"), "{} has &nbsp;", chapter.id);
        for tag in ["html", "head", "body", "div", "p", "a", "blockquote"] {
            let opens = xhtml.matches(&format!("<{tag}")).count();
            let closes = xhtml.matches(&format!("</{tag}>")).count();
            assert_eq!(opens, closes, "unbalanced <{tag}> in {}", chapter.id);
        }
        // Void elements are self-closed.
        for void in ["<br>", "<hr>", "<link ", "<meta charset=\"utf-8\">"] {
            assert!(!xhtml.contains(void) || xhtml.contains("/>"), "{void}");
        }
        // Every `&` opens an entity XML actually defines.
        for (i, _) in xhtml.match_indices('&') {
            let tail = &xhtml[i + 1..];
            assert!(
                is_defined_entity(tail),
                "bare `&` at byte {i} in {}: {:?}",
                chapter.id,
                &xhtml[i..(i + 24).min(xhtml.len())]
            );
        }
    }
}

/// XML predefines only these five names; everything else must be numeric.
fn is_defined_entity(after_ampersand: &str) -> bool {
    let Some(end) = after_ampersand.find(';') else {
        return false;
    };
    let name = &after_ampersand[..end];
    if matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") {
        return true;
    }
    match name.strip_prefix('#') {
        Some(rest) => match rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
            Some(hex) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
            None => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        },
        None => false,
    }
}

#[test]
fn rating_links_carry_the_spec_token() {
    let issue = fixtures::issue();
    let chapter = build::render_all(
        &issue,
        Edition::Standard,
        &[],
        "https://daily.hallada.net",
        Some("integration-secret"),
    )
    .expect("render")
    .into_iter()
    .find(|c| c.id == "art-1001")
    .expect("article chapter");

    let date = issue.meta.date;
    let loved = build::rating_token("integration-secret", date, 1, Vote::Loved);
    let good = build::rating_token("integration-secret", date, 1, Vote::Good);
    let down = build::rating_token("integration-secret", date, 1, Vote::NotForMe);
    assert_eq!(loved.len(), 16);
    assert_ne!(loved, good);
    assert_ne!(good, down);
    for (segment, token) in [("loved", loved), ("good", good), ("down", down)] {
        assert!(chapter.xhtml.contains(&format!(
            "https://daily.hallada.net/r/2026-08-15/1/{segment}?t={token}"
        )));
    }
    assert!(chapter.xhtml.contains("[ Loved it ]"));
    assert!(chapter.xhtml.contains("[ Good ]"));
    assert!(chapter.xhtml.contains("[ Not for me ]"));
}

#[test]
fn comment_and_world_fixtures_feed_real_chapters() {
    let hn: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hn_item.json"
        ))
        .expect("fixture"),
    )
    .expect("json");
    let thread = comments::parse_hn(&hn).expect("thread");
    assert_eq!(thread.total_comments, 4);

    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/wikipedia_current_events.html"
    ))
    .expect("fixture");
    let sections = world::extract_events(
        &html,
        "https://en.wikipedia.org/wiki/Portal:Current_events/2026_August_14",
    )
    .expect("events");
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0].events[0].children.len(), 1);
    assert!(!sections[0].events[0].links.is_empty());
}

#[test]
fn colophon_facts_are_x4_safe_distinct_paragraphs() {
    let issue = fixtures::issue();
    for edition in [Edition::Standard, Edition::X4] {
        let (_dir, _, zip) = build_edition_to_bytes(&issue, edition);
        let colophon = read_entry(&zip, "OEBPS/colophon.xhtml");
        assert_eq!(colophon.matches("<p class=\"fact-line\">").count(), 9);
        assert!(!colophon.contains("<dl"));
        assert!(!colophon.contains("<dt"));
        assert!(!colophon.contains("<dd"));
        assert!(colophon.contains("<strong>Issue:</strong>"));
        assert!(colophon.contains("<strong>Generator:</strong>"));
    }
}
