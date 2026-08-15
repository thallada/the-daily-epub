//! Publishing: BookOrbit watched folder, XTC delivery, OPDS feed, retention
//! (spec §3.11).
//!
//! Everything here is deliberately dumb about *how* artifacts were produced: the
//! EPUB/XTC stages hand over finished files, this module only copies, indexes and
//! prunes them. Copies are atomic (temp file in the destination directory, then
//! `rename`) so BookOrbit's watcher and CrossPoint's OPDS client never observe a
//! half-written book.
//!
//! [`crate::pipeline`] ends a non-dry run with one call —
//! `publish_issue(db, config, &issue, &artifacts, xtc_path.as_deref())`, where
//! `artifacts` are the `epub::build_all` outputs and `xtc_path` is
//! `epub::x4::convert`'s output (`None` when the converter is disabled or
//! failed) — and feeds the returned [`Published`] paths into
//! `db.upsert_issue(..., epub_path, x4_path, xtc_path, ...)`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use jiff::{Timestamp, Zoned};
use sqlx::Row;
use tokio::io::AsyncWriteExt;

use crate::config::Config;
use crate::db::Db;
use crate::types::{Artifact, Edition, Issue};

/// Filename of the generated static OPDS feed (§3.11).
pub const XTC_OPDS_FILENAME: &str = "xtc.xml";
/// Number of issues listed in the XTC OPDS feed (§3.11).
pub const XTC_FEED_ENTRIES: usize = 14;
/// Every published file starts with this (the retention sweep keys off it).
pub const FILE_PREFIX: &str = "The Daily EPUB - ";
/// Extensions the retention sweep is allowed to delete (§3.11).
pub const PRUNABLE_EXTENSIONS: [&str; 3] = ["epub", "xtc", "xtch"];

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("publish directory does not exist: {0}")]
    MissingDir(PathBuf),
}

impl PublishError {
    fn at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> PublishError {
        let path = path.into();
        move |source| PublishError::Io { path, source }
    }
}

/// Everything one `generate` run published (§3.11).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Published {
    /// The EPUB artifacts, rewritten to point at their published locations.
    pub epubs: Vec<Artifact>,
    /// The XTC artifact's published location, when the converter produced one.
    pub xtc: Option<PathBuf>,
    /// The regenerated OPDS feed.
    pub opds: Option<PathBuf>,
    /// How many expired files the retention sweep removed.
    pub pruned: usize,
}

/// Canonical published filename: `The Daily EPUB - 2026-08-15 (X4).epub` (§3.11).
pub fn issue_filename(date: Date, edition: Edition, extension: &str) -> String {
    format!("{FILE_PREFIX}{date}{}.{extension}", edition.file_suffix())
}

/// Parse the issue date back out of a published filename, `None` when the name
/// is not one of ours (the retention sweep must never touch foreign files).
pub fn date_from_filename(name: &str) -> Option<Date> {
    let rest = name.strip_prefix(FILE_PREFIX)?;
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    if !PRUNABLE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    rest.get(..10)?.parse::<Date>().ok()
}

// ---------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------

/// Atomic copy: write to a temp file in the destination dir, then rename (§3.11).
///
/// The temp file is created beside the destination so the rename stays within one
/// filesystem; a failed copy leaves the previous version of `dest` intact.
pub async fn atomic_copy(src: &Path, dest: &Path) -> Result<(), PublishError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(dir).await?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("daily-epub"),
        std::process::id(),
        Timestamp::now().as_nanosecond()
    ));

    let bytes = tokio::fs::read(src).await.map_err(PublishError::at(src))?;
    let write = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        Ok::<(), std::io::Error>(())
    };
    if let Err(source) = write.await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(PublishError::Io { path: tmp, source });
    }
    if let Err(source) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(PublishError::Io {
            path: dest.to_path_buf(),
            source,
        });
    }
    tracing::debug!(
        src = %src.display(),
        dest = %dest.display(),
        bytes = bytes.len(),
        "published atomically"
    );
    Ok(())
}

async fn ensure_dir(dir: &Path) -> Result<(), PublishError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(PublishError::at(dir))
}

/// Copy both EPUB editions into the BookOrbit watched folder (§3.11).
///
/// Returns the published paths in the same order as `artifacts`.
pub async fn publish_epubs(
    artifacts: &[Artifact],
    issue: &Issue,
    cfg: &Config,
) -> Result<Vec<PathBuf>, PublishError> {
    let dir = &cfg.publish.bookorbit_dir;
    ensure_dir(dir).await?;
    let mut published = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let extension = artifact
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("epub");
        let dest = dir.join(issue_filename(issue.meta.date, artifact.edition, extension));
        atomic_copy(&artifact.path, &dest).await?;
        tracing::info!(
            edition = ?artifact.edition,
            dest = %dest.display(),
            bytes = artifact.bytes,
            "published edition to the BookOrbit library"
        );
        published.push(dest);
    }
    Ok(published)
}

/// Copy the `.xtc`/`.xtch` artifact into `publish.xtc_dir` (§3.11).
pub async fn publish_xtc(xtc: &Path, cfg: &Config) -> Result<PathBuf, PublishError> {
    let dir = &cfg.publish.xtc_dir;
    ensure_dir(dir).await?;
    let name = xtc
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("daily-epub.xtch");
    let dest = dir.join(name);
    atomic_copy(xtc, &dest).await?;
    tracing::info!(dest = %dest.display(), "published XTC artifact");
    Ok(dest)
}

/// Publish everything one run produced, refresh the OPDS feed and prune (§3.11).
///
/// `xtc` is `None` when the converter is disabled or failed — that is not an
/// error, the X4 falls back to the EPUB edition from BookOrbit.
pub async fn publish_issue(
    db: &Db,
    cfg: &Config,
    issue: &Issue,
    artifacts: &[Artifact],
    xtc: Option<&Path>,
) -> Result<Published, PublishError> {
    let span = tracing::info_span!("publish", date = %issue.meta.date);
    let _guard = span.enter();

    let paths = publish_epubs(artifacts, issue, cfg).await?;
    let epubs = artifacts
        .iter()
        .zip(paths)
        .map(|(artifact, path)| Artifact {
            path,
            ..artifact.clone()
        })
        .collect();

    let xtc = match xtc {
        Some(src) => Some(publish_xtc(src, cfg).await?),
        None => None,
    };
    let opds = Some(write_xtc_opds(db, cfg).await?);
    let pruned = prune(cfg, issue.meta.date).await?;

    Ok(Published {
        epubs,
        xtc,
        opds,
        pruned,
    })
}

// ---------------------------------------------------------------------------
// OPDS 1.2 acquisition feed (§3.11)
// ---------------------------------------------------------------------------

/// One published XTC file, as listed in the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct XtcFile {
    name: String,
    date: Option<Date>,
    modified: Timestamp,
    bytes: u64,
}

/// Regenerate the static OPDS 1.2 acquisition feed for the XTC directory:
/// newest first, last [`XTC_FEED_ENTRIES`], entries typed
/// `application/octet-stream` (§3.11).
pub async fn write_xtc_opds(db: &Db, cfg: &Config) -> Result<PathBuf, PublishError> {
    let dir = &cfg.publish.xtc_dir;
    ensure_dir(dir).await?;
    let files = scan_xtc_dir(dir).await?;
    let numbers = issue_numbers(db, &files).await;
    let feed = render_opds(&files, &numbers, &cfg.server.public_url, Timestamp::now());

    let dest = dir.join(XTC_OPDS_FILENAME);
    write_atomic(&dest, feed.as_bytes()).await?;
    tracing::info!(entries = files.len(), dest = %dest.display(), "wrote the XTC OPDS feed");
    Ok(dest)
}

/// XTC artifacts in `dir`, newest first, capped at [`XTC_FEED_ENTRIES`].
async fn scan_xtc_dir(dir: &Path) -> Result<Vec<XtcFile>, PublishError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(PublishError::at(dir))?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(PublishError::at(dir))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let extension = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "xtc" | "xtch") {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, name, "skipping unreadable XTC file");
                continue;
            }
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| Timestamp::try_from(m).ok())
            .unwrap_or_else(Timestamp::now);
        files.push(XtcFile {
            date: date_from_filename(&name),
            name,
            modified,
            bytes: meta.len(),
        });
    }
    // Newest first: by issue date when the filename carries one, else by mtime.
    files.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then(b.modified.cmp(&a.modified))
            .then(a.name.cmp(&b.name))
    });
    files.truncate(XTC_FEED_ENTRIES);
    Ok(files)
}

/// Issue numbers for the dated files, best-effort (the feed is still valid
/// without them). Uses the `db` escape hatch — no bespoke helper in `db.rs`.
async fn issue_numbers(db: &Db, files: &[XtcFile]) -> BTreeMap<Date, i64> {
    let mut numbers = BTreeMap::new();
    for date in files.iter().filter_map(|f| f.date) {
        let row = sqlx::query("SELECT issue_number FROM issues WHERE date = ?")
            .bind(date.to_string())
            .fetch_optional(db.pool())
            .await;
        match row {
            Ok(Some(row)) => {
                numbers.insert(date, row.get::<i64, _>("issue_number"));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, %date, "issue number lookup failed"),
        }
    }
    numbers
}

/// Render the Atom/OPDS document (§3.11).
fn render_opds(
    files: &[XtcFile],
    numbers: &BTreeMap<Date, i64>,
    public_url: &str,
    now: Timestamp,
) -> String {
    let base = public_url.trim_end_matches('/');
    let self_href = format!("{base}/opds/xtc.xml");
    let updated = files.first().map(|f| f.modified).unwrap_or(now);

    let mut out = String::with_capacity(1024 + files.len() * 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" \
xmlns:dc=\"http://purl.org/dc/terms/\" \
xmlns:opds=\"http://opds-spec.org/2010/catalog\">\n",
    );
    out.push_str("  <id>urn:daily-epub:xtc</id>\n");
    out.push_str("  <title>The Daily EPUB — XTC editions</title>\n");
    out.push_str(&format!("  <updated>{}</updated>\n", rfc3339(updated)));
    out.push_str("  <author><name>The Daily EPUB</name></author>\n");
    out.push_str(&format!(
        "  <link rel=\"self\" href=\"{}\" type=\"{}\"/>\n",
        xml_escape(&self_href),
        crate::server::OPDS_CONTENT_TYPE
    ));
    out.push_str(&format!(
        "  <link rel=\"start\" href=\"{}\" type=\"{}\"/>\n",
        xml_escape(&self_href),
        crate::server::OPDS_CONTENT_TYPE
    ));

    for file in files {
        let title = match file.date {
            Some(date) => format!("The Daily EPUB — {date}"),
            None => file.name.clone(),
        };
        let summary = match file.date.and_then(|d| numbers.get(&d)) {
            Some(n) => format!("Issue #{n} · {}", human_bytes(file.bytes)),
            None => human_bytes(file.bytes),
        };
        let href = format!("{base}/files/xtc/{}", percent_encode(&file.name));
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <title>{}</title>\n", xml_escape(&title)));
        out.push_str(&format!(
            "    <id>urn:daily-epub:xtc:{}</id>\n",
            xml_escape(&percent_encode(&file.name))
        ));
        out.push_str(&format!(
            "    <updated>{}</updated>\n",
            rfc3339(file.modified)
        ));
        if let Some(date) = file.date {
            out.push_str(&format!("    <dc:issued>{date}</dc:issued>\n"));
        }
        out.push_str("    <author><name>The Daily EPUB</name></author>\n");
        out.push_str(&format!(
            "    <summary>{}</summary>\n",
            xml_escape(&summary)
        ));
        out.push_str(&format!(
            "    <link rel=\"http://opds-spec.org/acquisition\" href=\"{}\" \
type=\"application/octet-stream\" length=\"{}\"/>\n",
            xml_escape(&href),
            file.bytes
        ));
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

/// Atom wants `1996-12-19T16:39:57-08:00`; jiff's `Timestamp` prints `…Z`.
fn rfc3339(ts: Timestamp) -> String {
    ts.to_string()
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", (bytes as f64 / 1024.0).ceil())
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode one URL path segment (filenames contain spaces and parens).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), PublishError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("daily-epub"),
        std::process::id()
    ));
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(PublishError::at(&tmp))?;
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(PublishError::at(dest))
}

// ---------------------------------------------------------------------------
// Retention (§3.11)
// ---------------------------------------------------------------------------

/// Delete issue files older than `retention_days` from both publish dirs.
/// SQLite history is kept forever — it's the training data (§3.11).
///
/// Only files named `The Daily EPUB - YYYY-MM-DD*.{epub,xtc,xtch}` are ever
/// considered; anything else in those directories (including `xtc.xml` and other
/// people's books) is left strictly alone.
pub async fn prune(cfg: &Config, today: Date) -> Result<usize, PublishError> {
    let cutoff = today
        .checked_sub(jiff::Span::new().days(i64::from(cfg.retention_days)))
        .unwrap_or(today);
    let mut removed = 0;
    for dir in [&cfg.publish.bookorbit_dir, &cfg.publish.xtc_dir] {
        removed += prune_dir(dir, cutoff).await?;
    }
    if removed > 0 {
        tracing::info!(removed, %cutoff, "retention sweep removed expired issues");
    }
    Ok(removed)
}

async fn prune_dir(dir: &Path, cutoff: Date) -> Result<usize, PublishError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(PublishError::at(dir))?;
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await.map_err(PublishError::at(dir))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(date) = date_from_filename(&name) else {
            continue;
        };
        if date >= cutoff {
            continue;
        }
        if !entry.metadata().await.map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                tracing::info!(path = %path.display(), %date, "pruned expired issue file");
                removed += 1;
            }
            Err(e) => tracing::warn!(error = %e, path = %path.display(), "could not prune file"),
        }
    }
    Ok(removed)
}

/// Today in the configured timezone — the reference point for [`prune`].
pub fn today_in_tz(cfg: &Config) -> Date {
    match cfg.tz() {
        Ok(tz) => Zoned::now().with_time_zone(tz).date(),
        Err(e) => {
            tracing::warn!(error = %e, "falling back to UTC for the retention cutoff");
            Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg(dir: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.publish.bookorbit_dir = dir.join("bookorbit");
        cfg.publish.xtc_dir = dir.join("xtc");
        cfg.server.public_url = "https://daily.hallada.net".into();
        cfg
    }

    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn filenames_match_the_spec() {
        assert_eq!(
            issue_filename(date("2026-08-15"), Edition::Standard, "epub"),
            "The Daily EPUB - 2026-08-15.epub"
        );
        assert_eq!(
            issue_filename(date("2026-08-15"), Edition::X4, "epub"),
            "The Daily EPUB - 2026-08-15 (X4).epub"
        );
        assert_eq!(
            issue_filename(date("2026-08-15"), Edition::X4, "xtch"),
            "The Daily EPUB - 2026-08-15 (X4).xtch"
        );
    }

    #[test]
    fn only_our_filenames_are_recognized() {
        assert_eq!(
            date_from_filename("The Daily EPUB - 2026-08-15.epub"),
            Some(date("2026-08-15"))
        );
        assert_eq!(
            date_from_filename("The Daily EPUB - 2026-08-15 (X4).xtch"),
            Some(date("2026-08-15"))
        );
        for foreign in [
            "xtc.xml",
            "Moby Dick.epub",
            "The Daily EPUB - notadate.epub",
            "The Daily EPUB - 2026-08-15.txt",
            "the daily epub - 2026-08-15.epub",
            "metadata.db",
        ] {
            assert_eq!(date_from_filename(foreign), None, "{foreign}");
        }
    }

    #[tokio::test]
    async fn atomic_copy_creates_the_final_name_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("build.epub");
        tokio::fs::write(&src, b"EPUB BYTES").await.unwrap();
        let dest = dir
            .path()
            .join("out")
            .join("The Daily EPUB - 2026-08-15.epub");

        atomic_copy(&src, &dest).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"EPUB BYTES");

        // Overwriting an existing issue works and stays atomic.
        tokio::fs::write(&src, b"REGENERATED").await.unwrap();
        atomic_copy(&src, &dest).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"REGENERATED");

        let leftovers: Vec<String> = std::fs::read_dir(dir.path().join("out"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        assert!(
            atomic_copy(Path::new("/nonexistent/x.epub"), &dest)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn publish_epubs_uses_canonical_names() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let std_src = dir.path().join("a.epub");
        let x4_src = dir.path().join("b.epub");
        tokio::fs::write(&std_src, b"standard").await.unwrap();
        tokio::fs::write(&x4_src, b"x4").await.unwrap();

        let artifacts = vec![
            Artifact {
                edition: Edition::Standard,
                path: std_src,
                bytes: 8,
            },
            Artifact {
                edition: Edition::X4,
                path: x4_src,
                bytes: 2,
            },
        ];
        let issue = fake_issue(date("2026-08-15"));
        let paths = publish_epubs(&artifacts, &issue, &cfg).await.unwrap();
        assert_eq!(
            paths,
            vec![
                cfg.publish
                    .bookorbit_dir
                    .join("The Daily EPUB - 2026-08-15.epub"),
                cfg.publish
                    .bookorbit_dir
                    .join("The Daily EPUB - 2026-08-15 (X4).epub"),
            ]
        );
        assert_eq!(tokio::fs::read(&paths[0]).await.unwrap(), b"standard");

        let xtc_src = dir.path().join("c.xtch");
        tokio::fs::write(&xtc_src, b"xtch").await.unwrap();
        let published = publish_xtc(&xtc_src, &cfg).await.unwrap();
        assert_eq!(published, cfg.publish.xtc_dir.join("c.xtch"));
    }

    fn fake_issue(date: Date) -> Issue {
        use crate::types::{Colophon, Editorial, IssueMeta, Lineup};
        Issue {
            meta: IssueMeta {
                date,
                issue_number: 12,
                generated_at: ts("2026-08-15T05:30:00Z"),
                display_date: "Saturday, August 15, 2026".into(),
                article_count: 3,
                section_count: 1,
                total_words: 900,
                reading_minutes: 5,
            },
            lineup: Lineup {
                date,
                picks: vec![],
                section_order: vec![],
            },
            editorial: Editorial::default(),
            world_briefing: None,
            colophon: Colophon::default(),
        }
    }

    #[test]
    fn opds_feed_is_newest_first_with_acquisition_links() {
        let files = vec![
            XtcFile {
                name: "The Daily EPUB - 2026-08-15 (X4).xtch".into(),
                date: Some(date("2026-08-15")),
                modified: ts("2026-08-15T05:40:00Z"),
                bytes: 2_500_000,
            },
            XtcFile {
                name: "The Daily EPUB - 2026-08-14 (X4).xtch".into(),
                date: Some(date("2026-08-14")),
                modified: ts("2026-08-14T05:40:00Z"),
                bytes: 4096,
            },
        ];
        let mut numbers = BTreeMap::new();
        numbers.insert(date("2026-08-15"), 12);
        let feed = render_opds(
            &files,
            &numbers,
            "https://daily.hallada.net/",
            ts("2026-08-15T06:00:00Z"),
        );

        assert!(feed.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(feed.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\""));
        assert!(feed.contains("<id>urn:daily-epub:xtc</id>"));
        assert!(feed.contains("<updated>2026-08-15T05:40:00Z</updated>"));
        assert_eq!(feed.matches("<entry>").count(), 2);
        assert_eq!(feed.matches("</entry>").count(), 2);
        // Newest first.
        let i15 = feed.find("The Daily EPUB — 2026-08-15").unwrap();
        let i14 = feed.find("The Daily EPUB — 2026-08-14").unwrap();
        assert!(i15 < i14);
        // Acquisition link, encoded filename, absolute public URL, size.
        assert!(feed.contains(
            "<link rel=\"http://opds-spec.org/acquisition\" \
href=\"https://daily.hallada.net/files/xtc/The%20Daily%20EPUB%20-%202026-08-15%20%28X4%29.xtch\" \
type=\"application/octet-stream\" length=\"2500000\"/>"
        ));
        assert!(feed.contains("Issue #12 · 2.4 MB"));
        assert!(feed.trim_end().ends_with("</feed>"));
        assert!(!feed.contains("&<"), "unescaped markup leaked in");
    }

    #[tokio::test]
    async fn write_xtc_opds_lists_only_xtc_files_capped_at_fourteen() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        std::fs::create_dir_all(&cfg.publish.xtc_dir).unwrap();
        for day in 1..=20 {
            let name = format!("The Daily EPUB - 2026-08-{day:02} (X4).xtch");
            std::fs::write(cfg.publish.xtc_dir.join(name), b"x").unwrap();
        }
        // Non-XTC neighbours must be ignored.
        std::fs::write(cfg.publish.xtc_dir.join("README.txt"), b"x").unwrap();
        std::fs::write(cfg.publish.xtc_dir.join("cover.epub"), b"x").unwrap();

        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        db.upsert_issue(
            date("2026-08-20"),
            20,
            ts("2026-08-20T05:30:00Z"),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let path = write_xtc_opds(&db, &cfg).await.unwrap();
        assert_eq!(path, cfg.publish.xtc_dir.join(XTC_OPDS_FILENAME));
        let feed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(feed.matches("<entry>").count(), XTC_FEED_ENTRIES);
        assert!(feed.contains("2026-08-20"));
        assert!(!feed.contains("2026-08-06"), "older than the last 14");
        assert!(!feed.contains("README"));
        assert!(!feed.contains("cover.epub"));
        assert!(feed.contains("Issue #20"));

        // Regenerating replaces the file in place.
        write_xtc_opds(&db, &cfg).await.unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&cfg.publish.xtc_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn prune_only_deletes_old_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut conf = cfg(dir.path());
        conf.retention_days = 21;
        std::fs::create_dir_all(&conf.publish.bookorbit_dir).unwrap();
        std::fs::create_dir_all(&conf.publish.xtc_dir).unwrap();

        let keep_epub = conf
            .publish
            .bookorbit_dir
            .join("The Daily EPUB - 2026-08-14.epub");
        let old_epub = conf
            .publish
            .bookorbit_dir
            .join("The Daily EPUB - 2026-07-01.epub");
        let old_x4 = conf
            .publish
            .bookorbit_dir
            .join("The Daily EPUB - 2026-07-01 (X4).epub");
        let foreign = conf.publish.bookorbit_dir.join("Moby Dick.epub");
        let old_xtc = conf
            .publish
            .xtc_dir
            .join("The Daily EPUB - 2026-07-01 (X4).xtch");
        let feed = conf.publish.xtc_dir.join(XTC_OPDS_FILENAME);
        for path in [&keep_epub, &old_epub, &old_x4, &foreign, &old_xtc, &feed] {
            std::fs::write(path, b"x").unwrap();
        }

        let removed = prune(&conf, date("2026-08-15")).await.unwrap();
        assert_eq!(removed, 3);
        assert!(keep_epub.exists());
        assert!(foreign.exists(), "never touch other people's books");
        assert!(feed.exists(), "the OPDS feed is not an issue file");
        assert!(!old_epub.exists());
        assert!(!old_x4.exists());
        assert!(!old_xtc.exists());

        // Idempotent, and tolerant of missing directories.
        assert_eq!(prune(&conf, date("2026-08-15")).await.unwrap(), 0);
        let missing_dirs = cfg(&dir.path().join("nowhere"));
        assert_eq!(prune(&missing_dirs, date("2026-08-15")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn publish_issue_does_the_whole_dance() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let src = dir.path().join("std.epub");
        let x4_src = dir.path().join("x4.epub");
        let xtc_src = dir.path().join("out.xtch");
        for (path, body) in [
            (&src, "standard"),
            (&x4_src, "x4"),
            (&xtc_src, "xtch-bytes"),
        ] {
            std::fs::write(path, body).unwrap();
        }
        let artifacts = vec![
            Artifact {
                edition: Edition::Standard,
                path: src,
                bytes: 8,
            },
            Artifact {
                edition: Edition::X4,
                path: x4_src,
                bytes: 2,
            },
        ];
        let issue = fake_issue(date("2026-08-15"));

        let published = publish_issue(&db, &cfg, &issue, &artifacts, Some(&xtc_src))
            .await
            .unwrap();
        assert_eq!(published.epubs.len(), 2);
        assert!(published.epubs.iter().all(|a| a.path.exists()));
        assert_eq!(published.epubs[1].edition, Edition::X4);
        assert!(published.xtc.as_ref().is_some_and(|p| p.exists()));
        assert!(published.opds.as_ref().is_some_and(|p| p.exists()));
        assert_eq!(published.pruned, 0);

        let feed = std::fs::read_to_string(cfg.publish.xtc_dir.join(XTC_OPDS_FILENAME)).unwrap();
        assert!(feed.contains("out.xtch"));

        // No XTC artifact is fine — the feed is still regenerated.
        let published = publish_issue(&db, &cfg, &issue, &artifacts, None)
            .await
            .unwrap();
        assert!(published.xtc.is_none());
        assert!(published.opds.is_some());
    }

    #[test]
    fn helpers_escape_and_encode() {
        assert_eq!(xml_escape("a & b < c"), "a &amp; b &lt; c");
        assert_eq!(percent_encode("a b(c).xtch"), "a%20b%28c%29.xtch");
        assert_eq!(human_bytes(2_500_000), "2.4 MB");
        assert_eq!(human_bytes(4096), "4 KB");
    }
}
