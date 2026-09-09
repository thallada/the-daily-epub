//! Real-world image audit: replay the extraction + image pipeline over the
//! articles of already-published issues and report what reaches the page.
//!
//! Unit tests can only prove the code does what we think on markup we wrote.
//! This runs the same code over the pages the issues were actually built from,
//! so a regression in lazy-image handling, URL matching or re-encoding shows up
//! as a number rather than a hunch.
//!
//! ```text
//! cargo run --release --example image_audit -- \
//!     --cache /tmp/pagecache ~/bookorbit/books/daily-epub/*.epub
//! ```
//!
//! Pages are cached on disk after the first run, so before/after comparisons
//! see byte-identical input. Articles are re-fetched and re-extracted through
//! readability even when the original issue took its body from Miniflux, so the
//! counts describe the fetch path, not that specific issue's history.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use daily_epub::epub::build;
use daily_epub::extract::{self, Extractor};
use daily_epub::html;
use daily_epub::images;
use daily_epub::types::{
    Article, Colophon, Editorial, EntryId, ExtractMethod, ImageAsset, Issue, IssueMeta, Lineup,
    Pick, SourceKind, SourceRef,
};

/// One article we are auditing, pulled back out of a published EPUB.
#[derive(Debug, Clone)]
struct Target {
    issue: String,
    entry_id: EntryId,
    title: String,
    url: String,
}

/// What the pipeline did with one article's images.
#[derive(Debug, Default, Clone)]
struct Outcome {
    /// `<img>` elements in the extracted body.
    refs: usize,
    /// Images that downloaded and re-encoded successfully.
    embedded: usize,
    /// `<img src="images/…">` in the rendered chapter.
    rendered: usize,
    /// `[image: …]` paragraphs in the rendered chapter.
    placeholders: usize,
    /// Reasons individual images did not make it, most specific first.
    losses: Vec<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .init();

    let mut cache = PathBuf::from("/tmp/daily-epub-audit-cache");
    let mut dump: Option<String> = None;
    let mut epub_out: Option<PathBuf> = None;
    let mut epubs: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cache" => cache = PathBuf::from(args.next().expect("--cache needs a path")),
            "--dump" => dump = Some(args.next().expect("--dump needs a title substring")),
            "--epub-out" => {
                epub_out = Some(PathBuf::from(args.next().expect("--epub-out needs a path")))
            }
            other => epubs.push(PathBuf::from(other)),
        }
    }
    if epubs.is_empty() {
        eprintln!(
            "usage: image_audit [--cache DIR] [--dump TITLE] [--epub-out DIR] <issue.epub>..."
        );
        std::process::exit(2);
    }
    std::fs::create_dir_all(&cache).expect("cache dir");

    // One entry per article: passing `*.epub` picks up both editions of every
    // issue, and the same article twice would collide on its asset ids.
    let mut seen: std::collections::HashSet<EntryId> = std::collections::HashSet::new();
    let targets: Vec<Target> = epubs
        .iter()
        .flat_map(|p| targets_from_epub(p))
        .filter(|t| seen.insert(t.entry_id))
        .collect();
    eprintln!(
        "auditing {} articles from {} files",
        targets.len(),
        epubs.len()
    );

    let http = daily_epub::http::build_client(std::time::Duration::from_secs(20)).unwrap();
    let extractor = Extractor::new(http.clone(), vec![]);

    // Extract every article first, then run one issue-wide image pass, exactly
    // as `epub::build_edition` does.
    let mut picks: Vec<Pick> = Vec::new();
    let mut skipped: Vec<(Target, String)> = Vec::new();
    for target in &targets {
        match body_for(&cache, &http, &extractor, target).await {
            Ok(html) => picks.push(pick_for(target, html)),
            Err(e) => skipped.push((target.clone(), e)),
        }
    }

    let assets =
        images::collect_for_issue(&http, &picks, daily_epub::types::Edition::Standard).await;
    eprintln!("embedded {} images", assets.len());

    let by_entry: BTreeMap<EntryId, Vec<&ImageAsset>> = assets.iter().fold(
        BTreeMap::new(),
        |mut acc: BTreeMap<EntryId, Vec<&ImageAsset>>, a| {
            if let Some(id) = entry_of(&a.id) {
                acc.entry(id).or_default().push(a);
            }
            acc
        },
    );

    let mut totals = Outcome::default();
    let mut rows: Vec<(Target, Outcome)> = Vec::new();
    for (target, pick) in targets_of(&targets, &picks) {
        let refs = images::extract_img_refs(&pick.article.content_html);
        let embedded = by_entry.get(&target.entry_id).map_or(0, |v| v.len());
        let body = build::prepare_body(&pick.article.content_html, &assets);
        let rendered = body.matches("<img src=\"images/").count();
        let placeholders = body.matches("image-placeholder").count();

        let embedded_urls: Vec<&str> = by_entry
            .get(&target.entry_id)
            .map(|v| v.iter().map(|a| a.source_url.as_str()).collect())
            .unwrap_or_default();
        let losses = refs
            .iter()
            .filter(|r| !embedded_urls.contains(&r.src.as_str()))
            .map(|r| classify(&r.src))
            .collect::<Vec<_>>();

        if dump.as_ref().is_some_and(|d| target.title.contains(d)) {
            println!("\n== {} [{}]", target.title, target.url);
            for r in &refs {
                let state = if embedded_urls.contains(&r.src.as_str()) {
                    "ok  "
                } else {
                    "MISS"
                };
                println!("  {state} {}", r.src);
            }
            println!("  --- rendered body images:");
            for cap in body.split("<img src=\"").skip(1) {
                println!("  {}", cap.split('"').next().unwrap_or_default());
            }
        }

        let outcome = Outcome {
            refs: refs.len(),
            embedded,
            rendered,
            placeholders,
            losses,
        };
        totals.refs += outcome.refs;
        totals.embedded += outcome.embedded;
        totals.rendered += outcome.rendered;
        totals.placeholders += outcome.placeholders;
        totals.losses.extend(outcome.losses.iter().cloned());
        rows.push((target.clone(), outcome));
    }

    println!(
        "\n{:<12} {:>5} {:>5} {:>5} {:>5}  article",
        "issue", "refs", "emb", "shown", "ph"
    );
    println!("{}", "-".repeat(100));
    for (t, o) in &rows {
        let flag = if o.placeholders > 0 || o.rendered < o.refs {
            "!"
        } else {
            " "
        };
        println!(
            "{:<12} {:>5} {:>5} {:>5} {:>5} {} {} [{}]",
            t.issue,
            o.refs,
            o.embedded,
            o.rendered,
            o.placeholders,
            flag,
            truncate(&t.title, 44),
            host(&t.url),
        );
        for loss in &o.losses {
            println!("{:>36}   lost: {loss}", "");
        }
    }

    println!("\n== totals");
    println!("  articles           {}", rows.len());
    println!("  <img> in bodies    {}", totals.refs);
    println!("  embedded           {}", totals.embedded);
    println!("  shown in chapters  {}", totals.rendered);
    println!("  placeholders       {}", totals.placeholders);
    println!(
        "  orphaned assets    {}",
        totals.embedded.saturating_sub(totals.rendered)
    );
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for loss in &totals.losses {
        *kinds.entry(loss.clone()).or_default() += 1;
    }
    println!("\n== loss reasons");
    for (kind, n) in &kinds {
        println!("  {n:>4}  {kind}");
    }
    if let Some(dir) = &epub_out {
        write_audit_epub(dir, &picks, &assets);
    }

    if !skipped.is_empty() {
        println!("\n== unfetchable ({})", skipped.len());
        for (t, e) in &skipped {
            println!("  {} — {e}", truncate(&t.title, 50));
        }
    }
}

/// Pair each target with the pick built from it (targets that failed to fetch
/// have no pick and are skipped).
fn targets_of<'a>(targets: &'a [Target], picks: &'a [Pick]) -> Vec<(&'a Target, &'a Pick)> {
    picks
        .iter()
        .filter_map(|p| {
            targets
                .iter()
                .find(|t| t.entry_id == p.article.best_entry_id)
                .map(|t| (t, p))
        })
        .collect()
}

fn entry_of(asset_id: &str) -> Option<EntryId> {
    asset_id
        .strip_prefix("img-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Why one `<img>` never became an embedded asset — a guess from its URL, good
/// enough to group the failures.
fn classify(src: &str) -> String {
    let lower = src.to_ascii_lowercase();
    if src.contains('{') || src.contains('}') {
        format!("unresolved URL template — {}", truncate(src, 90))
    } else if src.contains(' ') || src.contains("%20") {
        format!("srcset blob in src — {}", truncate(src, 90))
    } else if lower.contains(".svg") {
        format!("svg — {}", truncate(src, 90))
    } else if src.starts_with("data:") {
        "data: URI (lazy placeholder)".to_string()
    } else {
        format!("download/decode failed — {}", truncate(src, 90))
    }
}

/// Fetch + extract one article, using the on-disk page cache.
async fn body_for(
    cache: &Path,
    _http: &reqwest::Client,
    extractor: &Extractor,
    target: &Target,
) -> Result<String, String> {
    let key = cache.join(format!("{:x}.html", seahash(&target.url)));
    let url_key = key.with_extension("url");
    if !key.exists() {
        // `fetch_readable` does the fetching we want but returns readability's
        // output; cache the raw page instead so extraction changes are visible.
        let (raw, final_url) = raw_fetch(&target.url).await?;
        std::fs::write(&key, raw).map_err(|e| e.to_string())?;
        std::fs::write(&url_key, final_url).map_err(|e| e.to_string())?;
    }
    let bytes = std::fs::read(&key).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty page".into());
    }
    // Relative URLs belong to the page we landed on, not the one we asked for.
    let base = std::fs::read_to_string(&url_key).unwrap_or_else(|_| target.url.clone());
    let html = String::from_utf8_lossy(&bytes).into_owned();
    let _ = extractor;
    let readable = extract::readability(&html, &base).map_err(|e| e.to_string())?;
    Ok(extract::sanitize_with_base(
        &images::normalize_img_tags(&readable),
        &base,
    ))
}

/// A plain page fetch with the extractor's desktop UA and gzip handling,
/// returning the body and the URL the fetch landed on.
async fn raw_fetch(url: &str) -> Result<(Vec<u8>, String), String> {
    let client = reqwest::Client::builder()
        .user_agent(extract::DESKTOP_UA)
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let final_url = resp.url().to_string();
    resp.bytes()
        .await
        .map(|b| (b.to_vec(), final_url))
        .map_err(|e| e.to_string())
}

fn pick_for(target: &Target, content_html: String) -> Pick {
    let word_count = html::word_count(&content_html);
    let image_urls = images::collect_image_urls(&content_html, &target.url);
    Pick {
        article: Article {
            id: target.entry_id,
            canonical_url: target.url.clone(),
            title: target.title.clone(),
            best_entry_id: target.entry_id,
            content_html,
            word_count,
            excerpt_only: false,
            image_count: image_urls.len() as i64,
            image_urls,
            sources: vec![SourceRef {
                entry_id: target.entry_id,
                feed_id: 1,
                feed_title: target.issue.clone(),
                category: None,
                kind: SourceKind::Feed,
            }],
            first_seen: "2026-08-15T05:30:00Z".parse().unwrap(),
            url: target.url.clone(),
            author: None,
            publication: None,
            feed_id: 1,
            feed_title: host(&target.url),
            category: None,
            published_at: None,
            comments_url: None,
            social: vec![],
            extract_method: ExtractMethod::Readability,
        },
        section: target.issue.clone(),
        position: 0,
        is_lead: false,
        why: None,
        summary: None,
        llm: None,
        top_interests: Vec::new(),
        discussion: None,
    }
}

/// Build a readable EPUB out of the audited articles (`--epub-out DIR`).
///
/// Not a regenerated issue — there is no editorial, no discussion chapters and
/// no world briefing here, and the real thing needs the database. It exists so
/// the images can be looked at on a device rather than counted in a table.
fn write_audit_epub(out_dir: &Path, picks: &[Pick], assets: &[ImageAsset]) {
    let sections: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for p in picks {
            if !seen.contains(&p.section) {
                seen.push(p.section.clone());
            }
        }
        seen
    };
    let issue = Issue {
        meta: IssueMeta {
            date: "2026-08-16".parse().unwrap(),
            issue_number: 0,
            generated_at: jiff::Timestamp::now(),
            display_date: "Image audit rebuild".into(),
            article_count: picks.len() as i64,
            section_count: sections.len() as i64,
            total_words: picks.iter().map(|p| p.article.word_count).sum(),
            reading_minutes: picks.iter().map(|p| p.article.reading_minutes()).sum(),
        },
        lineup: Lineup {
            date: "2026-08-16".parse().unwrap(),
            picks: picks.to_vec(),
            section_order: sections,
        },
        editorial: Editorial {
            front_page_html: "<p>Rebuilt from published issues by \
                 <code>examples/image_audit.rs</code> to check image handling. \
                 Articles are re-extracted live; editorial, discussions and the \
                 world briefing are absent by design.</p>"
                .into(),
            summaries: Default::default(),
        },
        world_briefing: None,
        colophon: Colophon::default(),
        behind: Default::default(),
    };
    let cfg = daily_epub::config::Config::default();
    match daily_epub::epub::build_edition_with_images(
        &issue,
        daily_epub::types::Edition::Standard,
        &cfg,
        out_dir,
        assets,
    ) {
        Ok(artifact) => println!(
            "\nwrote {} ({:.1} MB, {} images)",
            artifact.path.display(),
            artifact.bytes as f64 / 1_048_576.0,
            assets.len()
        ),
        Err(e) => eprintln!("epub build failed: {e}"),
    }
}

/// Pull `(entry id, title, url)` out of every article chapter in an EPUB.
fn targets_from_epub(path: &Path) -> Vec<Target> {
    let issue = path
        .file_stem()
        .map(|s| s.to_string_lossy().replace("The Daily EPUB - ", ""))
        .unwrap_or_default();
    let file = std::fs::File::open(path).expect("open epub");
    let mut zip = zip::ZipArchive::new(file).expect("read epub");
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.contains("art-") && n.ends_with(".xhtml"))
        .collect();

    let mut out = Vec::new();
    for name in names {
        let mut xhtml = String::new();
        if zip
            .by_name(&name)
            .and_then(|mut f| f.read_to_string(&mut xhtml).map_err(Into::into))
            .is_err()
        {
            continue;
        }
        let Some(entry_id) = name
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_prefix("art-"))
            .and_then(|f| f.strip_suffix(".xhtml"))
            .and_then(|f| f.parse::<EntryId>().ok())
        else {
            continue;
        };
        let Some(url) = between(&xhtml, r#"class="read-online"><a href=""#, '"') else {
            continue;
        };
        let title = between(&xhtml, "<title>", '<').unwrap_or_else(|| "?".into());
        out.push(Target {
            issue: issue.clone(),
            entry_id,
            title: unescape(&title),
            url: unescape(&url),
        });
    }
    out
}

fn between(haystack: &str, prefix: &str, end: char) -> Option<String> {
    let start = haystack.find(prefix)? + prefix.len();
    let rest = &haystack[start..];
    let stop = rest.find(end)?;
    Some(rest[..stop].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// Tiny stable hash for cache filenames.
fn seahash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
