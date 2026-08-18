//! Downloading an issue's images and packing them into EPUB assets.

use std::time::Duration;

use futures::StreamExt;

use crate::types::{Edition, ImageAsset, Pick};

use super::encode::{ImageProfile, reencode};
use super::refs::{ImgRef, extract_img_refs};

/// Per-image download timeout (§3.10).
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 10;
/// Per-image size cap (§3.10).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Concurrent downloads (§3.10).
pub const CONCURRENCY: usize = 8;
/// Whole-issue asset budget (§3.10).
pub const ISSUE_ASSET_BUDGET_BYTES: usize = 25 * 1024 * 1024;

/// Download one image, honoring the timeout and size cap (§3.10).
pub async fn download(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = http
        .get(url)
        // Some CDNs answer `Accept: */*` with an HTML interstitial (§3.10).
        .header(reqwest::header::ACCEPT, "image/*,*/*;q=0.8")
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| tracing::debug!(url, "image download failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(url, status = %resp.status(), "image download rejected");
        return None;
    }
    if let Some(len) = resp.content_length()
        && len as usize > MAX_IMAGE_BYTES
    {
        tracing::debug!(url, len, "image exceeds the size cap");
        return None;
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
                    tracing::debug!(url, "image exceeds the size cap mid-stream");
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(url, "image download interrupted: {e}");
                return None;
            }
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// Everything needed to fetch one image, in deterministic issue order.
#[derive(Debug, Clone)]
struct PendingImage {
    id: String,
    url: String,
    alt: String,
    caption: Option<String>,
}

fn pending_for_pick(pick: &Pick) -> Vec<PendingImage> {
    let entry_id = pick.article.best_entry_id;
    let mut refs = extract_img_refs(&pick.article.content_html);
    if refs.is_empty() {
        refs = pick
            .article
            .image_urls
            .iter()
            .map(|u| ImgRef {
                src: u.clone(),
                alt: String::new(),
                caption: None,
            })
            .collect();
    }
    refs.into_iter()
        .filter(|r| r.src.starts_with("http://") || r.src.starts_with("https://"))
        .enumerate()
        .map(|(i, r)| PendingImage {
            id: format!("img-{entry_id}-{i}"),
            url: r.src,
            alt: r.alt,
            caption: r.caption,
        })
        .collect()
}

/// Download and re-encode every image referenced by the lineup for one edition,
/// respecting [`ISSUE_ASSET_BUDGET_BYTES`] (§3.10).
pub async fn collect_for_issue(
    http: &reqwest::Client,
    picks: &[Pick],
    edition: Edition,
) -> Vec<ImageAsset> {
    let profile = ImageProfile::for_edition(edition);
    let pending: Vec<PendingImage> = picks.iter().flat_map(pending_for_pick).collect();
    if pending.is_empty() {
        return Vec::new();
    }
    tracing::info!(count = pending.len(), ?edition, "downloading issue images");

    let results: Vec<Option<(PendingImage, Vec<u8>, &'static str)>> =
        futures::stream::iter(pending.into_iter().map(|p| {
            let http = http.clone();
            async move {
                let raw = download(&http, &p.url).await?;
                let (bytes, mime) = tokio::task::spawn_blocking(move || reencode(&raw, profile))
                    .await
                    .ok()
                    .flatten()?;
                Some((p, bytes, mime))
            }
        }))
        .buffered(CONCURRENCY)
        .collect()
        .await;

    let mut assets = Vec::new();
    let mut budget_used = 0usize;
    let mut skipped = 0usize;
    for result in results.into_iter().flatten() {
        let (pending, bytes, mime) = result;
        if budget_used + bytes.len() > ISSUE_ASSET_BUDGET_BYTES {
            skipped += 1;
            continue;
        }
        budget_used += bytes.len();
        let ext = if mime == "image/png" { "png" } else { "jpg" };
        assets.push(ImageAsset {
            href: format!("images/{}.{ext}", pending.id),
            id: pending.id,
            mime: mime.to_string(),
            data: bytes,
            alt: pending.alt,
            caption: pending.caption,
            source_url: pending.url,
        });
    }
    if skipped > 0 {
        tracing::warn!(skipped, budget_used, "issue image budget exhausted");
    }
    tracing::info!(
        embedded = assets.len(),
        bytes = budget_used,
        "issue images ready"
    );
    assets
}
