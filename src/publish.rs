//! Publishing: the EPUB library folder, XTC delivery, OPDS feed, retention
//! (spec §3.11).
//!
//! `publish.epub_dir` holds both EPUB editions and is what the OPDS feed lists;
//! BookOrbit may watch the same folder but nothing here depends on it.
//!
//! Everything here is deliberately dumb about *how* artifacts were produced: the
//! EPUB/XTC stages hand over finished files, this module only copies, indexes and
//! prunes them. Copies are atomic (temp file in the destination directory, then
//! `rename`) so BookOrbit's watcher and CrossPoint's OPDS client never observe a
//! half-written book.
//!
//! [`crate::pipeline`] ends a non-dry run with one call —
//! `publish_issue(config, &issue, &artifacts, xtc_path.as_deref())`, where
//! `artifacts` are the `epub::build_all` outputs and `xtc_path` is
//! `epub::x4::convert`'s output (`None` when the converter is disabled or
//! failed) — and feeds the returned [`Published`] paths into
//! `db.upsert_issue(..., epub_path, x4_path, xtc_path, ...)`.
//!
//! The OPDS feed ([`build_opds`]) is rendered per request by
//! [`crate::server`] rather than written here, and lists **EPUBs only** — see
//! its docs for why XTC cannot be delivered over OPDS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use jiff::{Timestamp, Zoned};
use sqlx::Row;
use tokio::io::AsyncWriteExt;

use crate::config::Config;
use crate::db::Db;
use crate::types::{Artifact, Edition, Issue};

/// Number of issue *days* listed in the OPDS feed; each contributes one entry
/// per edition (§3.11).
pub const OPDS_FEED_ISSUES: usize = 14;
/// Every published file starts with this (the retention sweep keys off it).
pub const FILE_PREFIX: &str = "The Daily EPUB - ";
/// Extensions the retention sweep is allowed to delete (§3.11).
pub const PRUNABLE_EXTENSIONS: [&str; 3] = ["epub", "xtc", "xtch"];
/// Extensions of the XTC artifacts, for the counted sweep of `xtc_dir` (§3.11).
pub const XTC_EXTENSIONS: [&str; 2] = ["xtc", "xtch"];
/// Canonical path of the OPDS feed, relative to `server.public_url` (§3.11).
pub const OPDS_PATH: &str = "/opds/daily.xml";
/// The one acquisition type CrossPoint's OPDS parser accepts (§3.11).
pub const EPUB_CONTENT_TYPE: &str = "application/epub+zip";

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

/// Copy both EPUB editions into `publish.epub_dir` (§3.11).
///
/// Returns the published paths in the same order as `artifacts`.
pub async fn publish_epubs(
    artifacts: &[Artifact],
    issue: &Issue,
    cfg: &Config,
) -> Result<Vec<PathBuf>, PublishError> {
    let dir = &cfg.publish.epub_dir;
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
            "published edition to the EPUB library"
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

/// Publish everything one run produced and prune (§3.11).
///
/// `xtc` is `None` when the converter is disabled or failed — that is not an
/// error, since the OPDS feed lists the EPUB editions either way.
pub async fn publish_issue(
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
    // The OPDS feed is rendered per request from the publish directory, so
    // there is nothing to write here (§3.11).
    let pruned = prune(cfg, issue.meta.date).await?;

    Ok(Published { epubs, xtc, pruned })
}

// ---------------------------------------------------------------------------
// OPDS 1.2 acquisition feed (§3.11)
// ---------------------------------------------------------------------------

/// One published EPUB, as listed in the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EpubFile {
    name: String,
    date: Option<Date>,
    edition: Edition,
    modified: Timestamp,
    bytes: u64,
}

/// Render the OPDS 1.2 acquisition feed for the EPUB publish directory (§3.11).
///
/// Rendered per request rather than written to disk: the directory is the only
/// source of truth, so the feed cannot go stale behind a failed publish, and
/// there is no generated file for the retention sweep to step around.
///
/// XTC artifacts are deliberately **not** listed. CrossPoint's OPDS browser only
/// acquires links typed `application/epub+zip` and always saves the result with
/// a `.epub` extension, which its reader dispatches on — so an XTC offered here
/// would either be invisible or download into a file that cannot be opened.
pub async fn build_opds(db: &Db, cfg: &Config) -> Result<String, PublishError> {
    let dir = &cfg.publish.epub_dir;
    ensure_dir(dir).await?;
    let files = scan_epub_dir(dir).await?;
    let numbers = issue_numbers(db, &files).await;
    Ok(render_opds(
        &files,
        &numbers,
        &cfg.server.public_url,
        Timestamp::now(),
    ))
}

/// Published EPUBs in `dir`, newest first, limited to the last
/// [`OPDS_FEED_ISSUES`] issue days (both editions of each).
async fn scan_epub_dir(dir: &Path) -> Result<Vec<EpubFile>, PublishError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(PublishError::at(dir))?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(PublishError::at(dir))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = Path::new(&name);
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        // Only our own issues: a shared library may hold other people's books.
        if extension != "epub" || date_from_filename(&name).is_none() {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, name, "skipping unreadable EPUB");
                continue;
            }
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| Timestamp::try_from(m).ok())
            .unwrap_or_else(Timestamp::now);
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        files.push(EpubFile {
            date: date_from_filename(&name),
            edition: Edition::from_file_stem(&stem),
            name,
            modified,
            bytes: meta.len(),
        });
    }
    // Newest issue first, and within an issue the standard edition leads.
    files.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then(a.edition.cmp(&b.edition))
            .then(b.modified.cmp(&a.modified))
            .then(a.name.cmp(&b.name))
    });
    // Cap by issue day, not by file: an issue is two entries and truncating
    // mid-issue would list one edition without the other.
    let mut kept_dates: Vec<Option<Date>> = Vec::new();
    files.retain(|file| {
        if !kept_dates.contains(&file.date) {
            kept_dates.push(file.date);
        }
        kept_dates.iter().position(|d| *d == file.date) < Some(OPDS_FEED_ISSUES)
    });
    Ok(files)
}

/// Issue numbers for the dated files, best-effort (the feed is still valid
/// without them). Uses the `db` escape hatch — no bespoke helper in `db.rs`.
async fn issue_numbers(db: &Db, files: &[EpubFile]) -> BTreeMap<Date, i64> {
    let mut numbers = BTreeMap::new();
    for date in files.iter().filter_map(|f| f.date) {
        if numbers.contains_key(&date) {
            continue;
        }
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
    files: &[EpubFile],
    numbers: &BTreeMap<Date, i64>,
    public_url: &str,
    now: Timestamp,
) -> String {
    let base = public_url.trim_end_matches('/');
    let self_href = format!("{base}{OPDS_PATH}");
    let updated = files.first().map(|f| f.modified).unwrap_or(now);

    let mut out = String::with_capacity(1024 + files.len() * 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" \
xmlns:dc=\"http://purl.org/dc/terms/\" \
xmlns:opds=\"http://opds-spec.org/2010/catalog\">\n",
    );
    out.push_str("  <id>urn:daily-epub:issues</id>\n");
    out.push_str("  <title>The Daily EPUB</title>\n");
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
        // Matches the EPUB's own `dc:title`, so the two editions of one issue
        // are told apart in the list rather than showing as the same book.
        let title = match file.date {
            Some(date) => format!("The Daily EPUB — {date}{}", file.edition.file_suffix()),
            None => file.name.clone(),
        };
        let edition_note = match file.edition {
            Edition::Standard => "Standard",
            Edition::X4 => "Xteink X4",
        };
        let summary = match file.date.and_then(|d| numbers.get(&d)) {
            Some(n) => format!("Issue #{n} · {edition_note} · {}", human_bytes(file.bytes)),
            None => format!("{edition_note} · {}", human_bytes(file.bytes)),
        };
        let href = format!("{base}/files/epub/{}", percent_encode(&file.name));
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <title>{}</title>\n", xml_escape(&title)));
        out.push_str(&format!(
            "    <id>urn:daily-epub:issue:{}</id>\n",
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
        // The type must be exactly `application/epub+zip`: CrossPoint's OPDS
        // parser compares it with `strcmp` and silently drops entries whose
        // acquisition link is anything else, reporting "No entries found".
        out.push_str(&format!(
            "    <link rel=\"http://opds-spec.org/acquisition\" href=\"{}\" \
type=\"{EPUB_CONTENT_TYPE}\" length=\"{}\"/>\n",
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
pub fn percent_encode(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// Retention (§3.11)
// ---------------------------------------------------------------------------

/// Retention sweep over both publish dirs. SQLite history is kept forever —
/// it's the training data (§3.11).
///
/// The two directories are swept on different rules: EPUBs age out after
/// `retention_days`, while XTC is capped at `xtc_retention_count` issues because
/// each one is ~80–100 MB of pre-rendered page bitmaps and the binding
/// constraint is disk rather than age.
///
/// Only files named `The Daily EPUB - YYYY-MM-DD*.{epub,xtc,xtch}` are ever
/// considered; anything else in those directories (other people's books) is left
/// strictly alone.
pub async fn prune(cfg: &Config, today: Date) -> Result<usize, PublishError> {
    let cutoff = today
        .checked_sub(jiff::Span::new().days(i64::from(cfg.retention_days)))
        .unwrap_or(today);
    let mut removed = prune_dir(&cfg.publish.epub_dir, cutoff).await?;
    if removed > 0 {
        tracing::info!(removed, %cutoff, "retention sweep removed expired EPUBs");
    }
    let xtc_removed = prune_xtc_dir(&cfg.publish.xtc_dir, cfg.xtc_retention_count as usize).await?;
    if xtc_removed > 0 {
        tracing::info!(
            removed = xtc_removed,
            keep = cfg.xtc_retention_count,
            "retention sweep trimmed the XTC directory"
        );
    }
    removed += xtc_removed;
    Ok(removed)
}

/// Keep only the newest `keep` XTC issues, deleting the rest (§3.11).
///
/// Counted rather than dated so the directory has a hard size ceiling no matter
/// how often `generate` runs.
async fn prune_xtc_dir(dir: &Path, keep: usize) -> Result<usize, PublishError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(PublishError::at(dir))?;
    let mut ours: Vec<(Date, PathBuf)> = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(PublishError::at(dir))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let extension = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !XTC_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let Some(date) = date_from_filename(&name) else {
            continue;
        };
        if !entry.metadata().await.map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        ours.push((date, entry.path()));
    }
    // Newest first, then drop everything past the cap.
    ours.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut removed = 0;
    for (date, path) in ours.into_iter().skip(keep) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                tracing::info!(path = %path.display(), %date, "pruned an XTC issue past the cap");
                removed += 1;
            }
            Err(e) => tracing::warn!(error = %e, path = %path.display(), "could not prune file"),
        }
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
        cfg.publish.epub_dir = dir.join("bookorbit");
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
            "The Daily EPUB - 2026-08-15.xml",
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
                    .epub_dir
                    .join("The Daily EPUB - 2026-08-15.epub"),
                cfg.publish
                    .epub_dir
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
            behind: Default::default(),
        }
    }

    fn epub_file(name: &str, edition: Edition, modified: &str, bytes: u64) -> EpubFile {
        EpubFile {
            date: date_from_filename(name),
            edition,
            name: name.into(),
            modified: ts(modified),
            bytes,
        }
    }

    #[test]
    fn opds_feed_is_newest_first_with_acquisition_links() {
        let files = vec![
            epub_file(
                "The Daily EPUB - 2026-08-15.epub",
                Edition::Standard,
                "2026-08-15T05:40:00Z",
                6_500_000,
            ),
            epub_file(
                "The Daily EPUB - 2026-08-15 (X4).epub",
                Edition::X4,
                "2026-08-15T05:40:00Z",
                1_700_000,
            ),
            epub_file(
                "The Daily EPUB - 2026-08-14.epub",
                Edition::Standard,
                "2026-08-14T05:40:00Z",
                4096,
            ),
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
        assert!(feed.contains("<id>urn:daily-epub:issues</id>"));
        assert!(feed.contains("<updated>2026-08-15T05:40:00Z</updated>"));
        assert_eq!(feed.matches("<entry>").count(), 3);
        assert_eq!(feed.matches("</entry>").count(), 3);

        // Newest issue first; the two editions of one issue are distinguishable
        // by title, matching each EPUB's own `dc:title`.
        let i15 = feed
            .find("<title>The Daily EPUB — 2026-08-15</title>")
            .unwrap();
        let i15_x4 = feed
            .find("<title>The Daily EPUB — 2026-08-15 (X4)</title>")
            .unwrap();
        let i14 = feed
            .find("<title>The Daily EPUB — 2026-08-14</title>")
            .unwrap();
        assert!(i15 < i15_x4 && i15_x4 < i14);

        // CrossPoint compares the acquisition type with `strcmp` against
        // `application/epub+zip` and drops the entry on any mismatch (§3.11).
        assert_eq!(
            feed.matches("type=\"application/epub+zip\"").count(),
            3,
            "{feed}"
        );
        assert!(!feed.contains("application/octet-stream"));
        assert!(feed.contains(
            "<link rel=\"http://opds-spec.org/acquisition\" \
href=\"https://daily.hallada.net/files/epub/The%20Daily%20EPUB%20-%202026-08-15%20%28X4%29.epub\" \
type=\"application/epub+zip\" length=\"1700000\"/>"
        ));
        assert!(feed.contains("Issue #12 · Standard · 6.2 MB"));
        assert!(feed.contains("Issue #12 · Xteink X4 · 1.6 MB"));
        assert!(
            feed.contains("<link rel=\"self\" href=\"https://daily.hallada.net/opds/daily.xml\"")
        );
        assert!(feed.trim_end().ends_with("</feed>"));
        assert!(!feed.contains("&<"), "unescaped markup leaked in");
    }

    #[tokio::test]
    async fn the_feed_lists_both_editions_of_the_last_fourteen_issues() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        std::fs::create_dir_all(&cfg.publish.epub_dir).unwrap();
        std::fs::create_dir_all(&cfg.publish.xtc_dir).unwrap();
        for day in 1..=20 {
            for edition in Edition::ALL {
                let name = issue_filename(date(&format!("2026-08-{day:02}")), edition, "epub");
                std::fs::write(cfg.publish.epub_dir.join(name), b"x").unwrap();
            }
        }
        // Other people's books and our own XTC artifacts must be ignored.
        std::fs::write(cfg.publish.epub_dir.join("Moby Dick.epub"), b"x").unwrap();
        std::fs::write(cfg.publish.epub_dir.join("metadata.db"), b"x").unwrap();
        std::fs::write(
            cfg.publish
                .xtc_dir
                .join("The Daily EPUB - 2026-08-20 (X4).xtch"),
            b"x",
        )
        .unwrap();

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
            None,
        )
        .await
        .unwrap();

        let feed = build_opds(&db, &cfg).await.unwrap();
        // Capped by issue day, so both editions of each of the last 14 survive.
        assert_eq!(feed.matches("<entry>").count(), OPDS_FEED_ISSUES * 2);
        assert!(feed.contains("2026-08-20"));
        assert!(!feed.contains("2026-08-06"), "older than the last 14");
        assert!(!feed.contains("Moby Dick"));
        assert!(!feed.contains("metadata.db"));
        assert!(feed.contains("Issue #20"));
        // XTC is never offered: CrossPoint cannot acquire it (§3.11).
        assert!(!feed.contains(".xtch"), "{feed}");
        assert!(!feed.contains("/files/xtc/"));
    }

    #[tokio::test]
    async fn prune_ages_out_epubs_but_counts_xtc() {
        let dir = tempfile::tempdir().unwrap();
        let mut conf = cfg(dir.path());
        conf.retention_days = 21;
        conf.xtc_retention_count = 5;
        std::fs::create_dir_all(&conf.publish.epub_dir).unwrap();
        std::fs::create_dir_all(&conf.publish.xtc_dir).unwrap();

        let keep_epub = conf
            .publish
            .epub_dir
            .join("The Daily EPUB - 2026-08-14.epub");
        let old_epub = conf
            .publish
            .epub_dir
            .join("The Daily EPUB - 2026-07-01.epub");
        let old_x4 = conf
            .publish
            .epub_dir
            .join("The Daily EPUB - 2026-07-01 (X4).epub");
        let foreign = conf.publish.epub_dir.join("Moby Dick.epub");
        for path in [&keep_epub, &old_epub, &old_x4, &foreign] {
            std::fs::write(path, b"x").unwrap();
        }

        // Eight consecutive XTC issues, all recent: age would keep every one,
        // the count cap keeps the newest five.
        let xtc: Vec<PathBuf> = (8..=15)
            .map(|day| {
                let path = conf
                    .publish
                    .xtc_dir
                    .join(format!("The Daily EPUB - 2026-08-{day:02} (X4).xtch"));
                std::fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        let foreign_xtc = conf.publish.xtc_dir.join("Someone Else.xtch");
        std::fs::write(&foreign_xtc, b"x").unwrap();

        // 2 expired EPUBs + 3 XTC issues past the cap of 5.
        let removed = prune(&conf, date("2026-08-15")).await.unwrap();
        assert_eq!(removed, 5);
        assert!(keep_epub.exists());
        assert!(foreign.exists(), "never touch other people's books");
        assert!(foreign_xtc.exists(), "nor their XTC files");
        assert!(!old_epub.exists());
        assert!(!old_x4.exists());
        // The five newest XTC issues (11th–15th) survive; 8th–10th are gone.
        for path in &xtc[..3] {
            assert!(!path.exists(), "{} should be pruned", path.display());
        }
        for path in &xtc[3..] {
            assert!(path.exists(), "{} should be kept", path.display());
        }

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

        let published = publish_issue(&cfg, &issue, &artifacts, Some(&xtc_src))
            .await
            .unwrap();
        assert_eq!(published.epubs.len(), 2);
        assert!(published.epubs.iter().all(|a| a.path.exists()));
        assert_eq!(published.epubs[1].edition, Edition::X4);
        assert!(published.xtc.as_ref().is_some_and(|p| p.exists()));
        assert_eq!(published.pruned, 0);

        // The feed is derived from the publish directory, so both editions show
        // up without publish having written anything.
        let feed = build_opds(&db, &cfg).await.unwrap();
        assert!(
            feed.contains("The Daily EPUB — 2026-08-15</title>"),
            "{feed}"
        );
        assert!(
            feed.contains("The Daily EPUB — 2026-08-15 (X4)</title>"),
            "{feed}"
        );
        assert!(!feed.contains("out.xtch"));

        // No XTC artifact is fine — the EPUBs are what the feed lists anyway.
        let published = publish_issue(&cfg, &issue, &artifacts, None).await.unwrap();
        assert!(published.xtc.is_none());
    }

    #[test]
    fn helpers_escape_and_encode() {
        assert_eq!(xml_escape("a & b < c"), "a &amp; b &lt; c");
        assert_eq!(percent_encode("a b(c).xtch"), "a%20b%28c%29.xtch");
        assert_eq!(human_bytes(2_500_000), "2.4 MB");
        assert_eq!(human_bytes(4096), "4 KB");
    }
}
