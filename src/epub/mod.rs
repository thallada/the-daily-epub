//! EPUB assembly (spec §3.10).
//!
//! Two editions per issue: `Standard` and `X4`. Both are fully offline (every
//! asset embedded), EPUB3 with a nav TOC + NCX fallback, chapter ids
//! `art-{entry_id}` so rating links stay stable across regenerations.

pub mod build;
pub mod images;
pub mod x4;

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::types::{Artifact, Edition, ImageAsset, Issue};

/// Chapter order inside an issue (§3.10).
pub const CHAPTER_ORDER: &[&str] = &[
    "cover",
    "from-the-editor",
    "in-this-issue",
    "sections",
    "world-briefing",
    "colophon",
];

#[derive(Debug, thiserror::Error)]
pub enum EpubError {
    #[error("epub build failed: {0}")]
    Build(String),
    #[error("template rendering failed: {0}")]
    Template(#[from] askama::Error),
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Output filename: `The Daily EPUB - 2026-08-15.epub` / `… (X4).epub` (§3.11).
pub fn output_filename(issue: &Issue, edition: Edition) -> String {
    format!(
        "The Daily EPUB - {}{}.epub",
        issue.meta.date,
        edition.file_suffix()
    )
}

/// Build one edition into `out_dir`, returning the written artifact (§3.10).
///
/// Downloads and re-encodes the issue's images first; everything else is offline.
pub async fn build_edition(
    issue: &Issue,
    edition: Edition,
    cfg: &Config,
    out_dir: &Path,
) -> Result<Artifact, EpubError> {
    let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT)
        .map_err(|e| EpubError::Build(format!("http client: {e}")))?;
    let assets = images::collect_for_issue(&http, &issue.lineup.picks, edition).await;
    build_edition_with_images(issue, edition, cfg, out_dir, &assets)
}

/// The offline half of [`build_edition`]: render, zip and write (§3.10).
pub fn build_edition_with_images(
    issue: &Issue,
    edition: Edition,
    cfg: &Config,
    out_dir: &Path,
    assets: &[ImageAsset],
) -> Result<Artifact, EpubError> {
    let span = tracing::info_span!("epub", %issue.meta.date, ?edition);
    let _guard = span.enter();

    let chapters = build::render_all(
        issue,
        edition,
        assets,
        &cfg.server.public_url,
        cfg.server.hmac_secret.as_deref(),
    )?;
    let cover = build::render_cover(issue, edition)?;
    let bytes = build::assemble(issue, edition, &chapters, assets, &cover)?;

    std::fs::create_dir_all(out_dir).map_err(|source| EpubError::Io {
        path: out_dir.to_path_buf(),
        source,
    })?;
    let path = out_dir.join(output_filename(issue, edition));
    // Write + rename so a reader (or BookOrbit's watcher) never sees a partial file.
    let tmp = path.with_extension("epub.part");
    std::fs::write(&tmp, &bytes).map_err(|source| EpubError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| EpubError::Io {
        path: path.clone(),
        source,
    })?;

    tracing::info!(
        path = %path.display(),
        bytes = bytes.len(),
        chapters = chapters.len(),
        images = assets.len(),
        "wrote edition"
    );
    Ok(Artifact {
        edition,
        path,
        bytes: bytes.len() as u64,
    })
}

/// Build both editions, returning the artifacts and how many images were
/// embedded across them (the run report records the count, §3.10, §3.13).
pub async fn build_all(
    issue: &Issue,
    cfg: &Config,
    out_dir: &Path,
) -> Result<(Vec<Artifact>, usize), EpubError> {
    let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT)
        .map_err(|e| EpubError::Build(format!("http client: {e}")))?;
    let mut artifacts = Vec::with_capacity(2);
    let mut embedded = 0;
    for edition in [Edition::Standard, Edition::X4] {
        // Downloaded per edition: the two editions need different resolutions
        // and colour profiles (§3.10 images).
        let assets = images::collect_for_issue(&http, &issue.lineup.picks, edition).await;
        embedded += assets.len();
        artifacts.push(build_edition_with_images(
            issue, edition, cfg, out_dir, &assets,
        )?);
    }
    Ok((artifacts, embedded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub::build::fixtures;

    /// Local file headers store entry names verbatim, so a byte search over the
    /// archive is enough to assert its contents without a zip reader.
    fn contains_entry(zip: &[u8], name: &str) -> bool {
        zip.windows(name.len()).any(|w| w == name.as_bytes())
    }

    #[test]
    fn output_filenames_follow_the_spec() {
        let issue = fixtures::issue();
        assert_eq!(
            output_filename(&issue, Edition::Standard),
            "The Daily EPUB - 2026-08-15.epub"
        );
        assert_eq!(
            output_filename(&issue, Edition::X4),
            "The Daily EPUB - 2026-08-15 (X4).epub"
        );
    }

    #[test]
    fn builds_a_complete_epub_for_both_editions() {
        let issue = fixtures::issue();
        let cfg = Config::default();
        let dir = tempfile::tempdir().expect("tempdir");

        for edition in [Edition::Standard, Edition::X4] {
            let artifact =
                build_edition_with_images(&issue, edition, &cfg, dir.path(), &[]).expect("build");
            assert_eq!(artifact.edition, edition);
            assert!(artifact.path.exists());
            assert!(artifact.bytes > 1000);

            let zip = std::fs::read(&artifact.path).expect("read epub");
            assert_eq!(&zip[0..4], b"PK\x03\x04", "is a zip");
            assert_eq!(&zip[30..38], b"mimetype", "mimetype is the first entry");
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
                "OEBPS/art-1001.xhtml",
                "OEBPS/disc-1001.xhtml",
                "OEBPS/art-1002.xhtml",
                "OEBPS/world.xhtml",
                "OEBPS/colophon.xhtml",
            ] {
                assert!(
                    contains_entry(&zip, entry),
                    "missing {entry} in {edition:?}"
                );
            }
            // No leftover temp file.
            assert!(
                !dir.path()
                    .join("The Daily EPUB - 2026-08-15.epub.part")
                    .exists()
            );
        }
    }
}
