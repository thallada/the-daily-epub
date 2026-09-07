//! Feed discovery: propose new Miniflux subscriptions from aggregator-only
//! articles (feed discovery plan, 2026-09-07).
//!
//! Most of the paper's stories arrive through an aggregator (HN, Lobsters,
//! Reddit, Scour) rather than the author's own feed, and every one of those is a
//! lead on a feed the operator does not subscribe to yet. [`run`] is a
//! best-effort pipeline stage (and the same function the `feeds discover` CLI
//! backfill calls) that asks Miniflux what feeds sit behind those articles,
//! **validates every answer itself**, and records the survivors in
//! `feed_candidates` for the dashboard to accept or dismiss.
//!
//! Miniflux's `POST /v1/discover` falls back to probing well-known paths and
//! returns every path that answered 200 without checking the body is a feed, so
//! a site that serves its SPA shell for any path yields up to nine bogus hits
//! (plan §2). Discover results are leads, not facts: [`sniff_feed`] decides.
//!
//! Like `imports`, this module owns its own SQL rather than growing `db.rs`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt as _;
use jiff::Timestamp;
use sqlx::Row as _;

use crate::config::DiscoveryConfig;
use crate::curate::telemetry::SignalsJson;
use crate::db::{Db, fmt_ts, parse_ts};
use crate::miniflux::{Discovered, FeedMeta, MinifluxClient, MinifluxError};
use crate::types::{Article, ArticleId, FeedId, SourceKind};

/// How long a checked host is left alone before it is looked up again (§3).
pub const RECHECK_DAYS: i64 = 90;
/// Most candidates kept from one host's discover results (§3).
const MAX_PER_HOST: usize = 3;
/// Validation fetches in flight across hosts (§3).
const CONCURRENCY: usize = 4;
/// Per-request timeout for a validation fetch (§3).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Never buffer more than this much of a body before sniffing it (§3).
const MAX_BODY_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One row of `feed_candidates`, plus the articles that led us to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: i64,
    pub feed_url: String,
    pub host: String,
    pub title: Option<String>,
    /// `candidate` | `added` | `dismissed`.
    pub status: String,
    pub first_seen: String,
    pub last_seen: String,
    pub miniflux_feed_id: Option<i64>,
    pub decided_at: Option<String>,
    /// Articles linked to this candidate, newest article id first.
    pub article_ids: Vec<ArticleId>,
}

const CANDIDATE_COLUMNS: &str =
    "id, feed_url, host, title, status, first_seen, last_seen, miniflux_feed_id, decided_at";

fn row_from(row: &sqlx::sqlite::SqliteRow) -> Candidate {
    Candidate {
        id: row.get("id"),
        feed_url: row.get("feed_url"),
        host: row.get("host"),
        title: row.get("title"),
        status: row.get("status"),
        first_seen: row.get("first_seen"),
        last_seen: row.get("last_seen"),
        miniflux_feed_id: row.get("miniflux_feed_id"),
        decided_at: row.get("decided_at"),
        article_ids: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Database helpers (plan §4 step 1)
// ---------------------------------------------------------------------------

/// When each of `hosts` was last looked up, and how many candidates it yielded.
pub async fn hosts_checked(db: &Db, hosts: &[String]) -> Result<HashMap<String, (Timestamp, i64)>> {
    let mut out = HashMap::new();
    for chunk in hosts.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT host, checked_at, candidates FROM feed_discovery_hosts
             WHERE host IN ({placeholders})"
        )));
        for host in chunk {
            query = query.bind(host);
        }
        for row in query.fetch_all(db.pool()).await? {
            let checked_at = parse_ts(
                "feed_discovery_hosts.checked_at",
                &row.get::<String, _>("checked_at"),
            )?;
            out.insert(
                row.get::<String, _>("host"),
                (checked_at, row.get::<i64, _>("candidates")),
            );
        }
    }
    Ok(out)
}

/// Memo that `host` was looked up, so it is not looked up again for
/// [`RECHECK_DAYS`].
pub async fn mark_host_checked(db: &Db, host: &str, candidates: i64, now: Timestamp) -> Result<()> {
    sqlx::query(
        "INSERT INTO feed_discovery_hosts (host, checked_at, candidates) VALUES (?, ?, ?)
         ON CONFLICT(host) DO UPDATE SET checked_at = excluded.checked_at,
                                         candidates = excluded.candidates",
    )
    .bind(host)
    .bind(fmt_ts(now))
    .bind(candidates)
    .execute(db.pool())
    .await?;
    Ok(())
}

async fn existing_candidate_id(db: &Db, feed_url: &str) -> Result<Option<i64>> {
    let row = sqlx::query("SELECT id FROM feed_candidates WHERE feed_url = ?")
        .bind(feed_url)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|row| row.get::<i64, _>("id")))
}

/// Insert `feed_url` as a candidate or bump its `last_seen`, returning its id.
///
/// A decision already taken is never undone: the `status` of an `added` or
/// `dismissed` row is left alone, and an existing title wins over a new one.
pub async fn upsert_candidate(
    db: &Db,
    feed_url: &str,
    host: &str,
    title: Option<&str>,
    now: Timestamp,
) -> Result<i64> {
    let now = fmt_ts(now);
    let row = sqlx::query(
        "INSERT INTO feed_candidates (feed_url, host, title, status, first_seen, last_seen)
         VALUES (?, ?, ?, 'candidate', ?, ?)
         ON CONFLICT(feed_url) DO UPDATE
             SET last_seen = excluded.last_seen,
                 title = COALESCE(feed_candidates.title, excluded.title)
         RETURNING id",
    )
    .bind(feed_url)
    .bind(host)
    .bind(title)
    .bind(&now)
    .bind(&now)
    .fetch_one(db.pool())
    .await?;
    Ok(row.get::<i64, _>("id"))
}

/// Record that `article_id` is evidence for `candidate_id`.
pub async fn link_article(db: &Db, candidate_id: i64, article_id: ArticleId) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO feed_candidate_articles (candidate_id, article_id) VALUES (?, ?)",
    )
    .bind(candidate_id)
    .bind(article_id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Ids of the still-undecided candidates on `host`.
pub async fn candidates_for_host(db: &Db, host: &str) -> Result<Vec<i64>> {
    let rows = sqlx::query(
        "SELECT id FROM feed_candidates WHERE host = ? AND status = 'candidate' ORDER BY id",
    )
    .bind(host)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(|row| row.get::<i64, _>("id")).collect())
}

/// Attach `article_ids` to every candidate in one statement.
async fn attach_article_ids(db: &Db, candidates: &mut [Candidate]) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let mut by_id: HashMap<i64, usize> = HashMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        by_id.insert(candidate.id, index);
    }
    let ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT candidate_id, article_id FROM feed_candidate_articles
             WHERE candidate_id IN ({placeholders}) ORDER BY article_id DESC"
        )));
        for id in chunk {
            query = query.bind(*id);
        }
        for row in query.fetch_all(db.pool()).await? {
            let candidate_id: i64 = row.get("candidate_id");
            if let Some(index) = by_id.get(&candidate_id) {
                candidates[*index].article_ids.push(row.get("article_id"));
            }
        }
    }
    Ok(())
}

/// One page of candidates with `status`, newest first, and the total count.
pub async fn list(
    db: &Db,
    status: &str,
    page: u32,
    per_page: u32,
) -> Result<(Vec<Candidate>, i64)> {
    let total = count(db, status).await?;
    let per_page = i64::from(per_page.max(1));
    let offset = i64::from(page.saturating_sub(1)) * per_page;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CANDIDATE_COLUMNS} FROM feed_candidates WHERE status = ?
         ORDER BY last_seen DESC, id DESC LIMIT ? OFFSET ?"
    )))
    .bind(status)
    .bind(per_page)
    .bind(offset)
    .fetch_all(db.pool())
    .await?;
    let mut candidates: Vec<Candidate> = rows.iter().map(row_from).collect();
    attach_article_ids(db, &mut candidates).await?;
    Ok((candidates, total))
}

/// Every undecided candidate — ranking sorts globally, so the page loads them
/// all and paginates in Rust (plan §4 step 4).
pub async fn all_candidates(db: &Db) -> Result<Vec<Candidate>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CANDIDATE_COLUMNS} FROM feed_candidates WHERE status = 'candidate'
         ORDER BY last_seen DESC, id DESC"
    )))
    .fetch_all(db.pool())
    .await?;
    let mut candidates: Vec<Candidate> = rows.iter().map(row_from).collect();
    attach_article_ids(db, &mut candidates).await?;
    Ok(candidates)
}

/// One candidate by id, with its linked articles.
pub async fn candidate(db: &Db, id: i64) -> Result<Option<Candidate>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {CANDIDATE_COLUMNS} FROM feed_candidates WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(db.pool())
    .await?;
    let Some(row) = row else { return Ok(None) };
    let mut found = vec![row_from(&row)];
    attach_article_ids(db, &mut found).await?;
    Ok(found.pop())
}

/// Record the operator's (or reconciliation's) decision on one candidate.
pub async fn set_status(
    db: &Db,
    id: i64,
    status: &str,
    miniflux_feed_id: Option<i64>,
    now: Timestamp,
) -> Result<()> {
    sqlx::query(
        "UPDATE feed_candidates
         SET status = ?,
             miniflux_feed_id = COALESCE(?, miniflux_feed_id),
             decided_at = ?
         WHERE id = ?",
    )
    .bind(status)
    .bind(miniflux_feed_id)
    .bind(fmt_ts(now))
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// How many candidates have `status`.
pub async fn count(db: &Db, status: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM feed_candidates WHERE status = ?")
            .bind(status)
            .fetch_one(db.pool())
            .await?,
    )
}

/// Flip candidates the operator subscribed to by other means to `added`.
///
/// A candidate matches a subscription when its feed URL is that feed's
/// `feed_url`, or when its host is the host of that feed's `feed_url` or
/// `site_url`. Returns how many rows changed.
pub async fn reconcile_added(
    db: &Db,
    subscribed: &HashMap<FeedId, FeedMeta>,
    now: Timestamp,
) -> Result<usize> {
    let mut by_url: HashMap<String, FeedId> = HashMap::new();
    let mut by_host: HashMap<String, FeedId> = HashMap::new();
    for (id, meta) in subscribed {
        by_url.insert(norm_url(&meta.feed_url), *id);
        for url in [&meta.feed_url, &meta.site_url] {
            if let Some(host) = host_of(url) {
                by_host.entry(host).or_insert(*id);
            }
        }
    }
    let mut reconciled = 0usize;
    for candidate in all_candidates(db).await? {
        let feed_id = by_url
            .get(&norm_url(&candidate.feed_url))
            .or_else(|| by_host.get(&candidate.host));
        if let Some(feed_id) = feed_id {
            set_status(db, candidate.id, "added", Some(*feed_id), now).await?;
            reconciled += 1;
            tracing::info!(
                candidate = candidate.id,
                feed_url = %candidate.feed_url,
                miniflux_feed_id = feed_id,
                "feed candidate is already subscribed; marking added"
            );
        }
    }
    Ok(reconciled)
}

// ---------------------------------------------------------------------------
// Pure helpers (plan §4 step 3)
// ---------------------------------------------------------------------------

/// True when no feed of the operator's carried this story — the only articles
/// worth a lookup (§3).
pub fn aggregator_only(article: &Article) -> bool {
    !article.came_via(SourceKind::Feed)
}

/// `https://WWW.Example.com/x` → `example.com`.
pub fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    (!host.is_empty()).then(|| host.to_string())
}

/// Comparison form of a feed URL: trimmed, lowercased, no trailing slash.
fn norm_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// True when `host` is `entry` or a subdomain of it.
fn host_matches(host: &str, entry: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
}

/// True when the operator asked never to look this host up (`skip_hosts`).
fn skipped(host: &str, skip_hosts: &[String]) -> bool {
    skip_hosts.iter().any(|entry| host_matches(host, entry))
}

/// Worth fetching at all? Comment feeds are noise (§3).
pub fn keep_result(result: &Discovered) -> bool {
    let url = result.url.to_ascii_lowercase();
    let title = result.title.to_ascii_lowercase();
    !url.is_empty() && !url.contains("comment") && !title.contains("comment")
}

/// What a validated body turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Rss,
    Atom,
    Rdf,
    Json,
}

/// Decide whether `body` really is a feed (§3).
///
/// Content type is useless here — SPAs answer `text/html` for every path and
/// real feeds turn up as `text/xml` or `application/octet-stream` — so the root
/// element decides, after an optional BOM, whitespace and XML declaration.
pub fn sniff_feed(body: &[u8]) -> Option<FeedKind> {
    let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(body);
    let mut rest = trim_start(body);
    if rest.starts_with(b"<?") {
        let end = rest.windows(2).position(|w| w == b"?>")?;
        rest = trim_start(&rest[end + 2..]);
    }
    let head: String = String::from_utf8_lossy(&rest[..rest.len().min(512)]).to_ascii_lowercase();
    if head.starts_with("<rss") {
        return Some(FeedKind::Rss);
    }
    if head.starts_with("<feed") {
        return Some(FeedKind::Atom);
    }
    if head.starts_with("<rdf:rdf") {
        return Some(FeedKind::Rdf);
    }
    if head.starts_with('{')
        && String::from_utf8_lossy(&rest[..rest.len().min(MAX_BODY_BYTES)])
            .contains("jsonfeed.org/version/")
    {
        return Some(FeedKind::Json);
    }
    None
}

fn trim_start(body: &[u8]) -> &[u8] {
    let start = body
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(body.len());
    &body[start..]
}

/// The feed's own `<title>`, used when Miniflux had no title to give (§3).
pub fn feed_title(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&body[..body.len().min(64 * 1024)]);
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title>")?;
    let raw = text[start..end]
        .trim()
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .trim();
    let title = unescape(raw);
    (!title.is_empty()).then_some(title)
}

/// The five entities a feed title realistically carries; a whole HTML entity
/// table is not worth a dependency here.
fn unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A discover result whose body sniffed as a feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub url: String,
    /// Miniflux's title for the result; `== url` when it was a well-known-path
    /// guess rather than a `<link rel=alternate>` hit.
    pub raw_title: String,
    /// `rss` | `atom` | `json`, as Miniflux reported it.
    pub kind: String,
    /// The `<title>` read out of the body, when we found one.
    pub sniffed_title: Option<String>,
}

impl Validated {
    /// True when this came from probing a well-known path, not a link tag (§2).
    pub fn is_guess(&self) -> bool {
        self.raw_title == self.url
    }

    /// What to store as the candidate's title.
    pub fn title(&self) -> Option<String> {
        if self.is_guess() {
            self.sniffed_title.clone()
        } else {
            Some(self.raw_title.clone())
        }
    }
}

/// Collapse one host's validated results into the few worth proposing (§3).
///
/// Link-tag hits are grouped by title (a site commonly offers the same feed as
/// Atom and JSON Feed under one title), preferring XML over JSON; at most one
/// well-known-path guess survives, since WordPress answers both `/feed/` and
/// `/rss/` with the same content. At most [`MAX_PER_HOST`] overall.
pub fn dedupe_guesses(results: Vec<Validated>) -> Vec<Validated> {
    let mut kept: Vec<Validated> = Vec::new();
    let mut titles: Vec<String> = Vec::new();
    for result in results.iter().filter(|r| !r.is_guess()) {
        match titles.iter().position(|t| t == &result.raw_title) {
            Some(index) => {
                if kept[index].kind.eq_ignore_ascii_case("json")
                    && !result.kind.eq_ignore_ascii_case("json")
                {
                    kept[index] = result.clone();
                }
            }
            None => {
                titles.push(result.raw_title.clone());
                kept.push(result.clone());
            }
        }
    }
    if let Some(guess) = results.into_iter().find(Validated::is_guess) {
        kept.push(guess);
    }
    kept.truncate(MAX_PER_HOST);
    kept
}

// ---------------------------------------------------------------------------
// The stage (plan §4 step 3)
// ---------------------------------------------------------------------------

/// What one discovery pass did, for the report line and the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Hosts looked up in Miniflux this pass (an error response counts: the
    /// host is memoized either way).
    pub hosts_checked: usize,
    /// Hosts skipped because the operator already subscribes to them.
    pub hosts_skipped_subscribed: usize,
    /// Hosts whose memo was still fresh, so their articles were merely linked.
    pub hosts_reused: usize,
    /// Discover results that did not sniff as a feed.
    pub results_rejected: usize,
    /// Candidates inserted for the first time.
    pub candidates_new: usize,
    /// Article ↔ candidate links written.
    pub articles_linked: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} hosts checked, {} reused, {} already subscribed · {} results rejected · \
             {} new candidates, {} articles linked",
            self.hosts_checked,
            self.hosts_reused,
            self.hosts_skipped_subscribed,
            self.results_rejected,
            self.candidates_new,
            self.articles_linked,
        )
    }
}

/// One host queued for a Miniflux lookup.
#[derive(Debug, Clone)]
struct Lookup {
    host: String,
    /// The article URL to discover from — link tags only exist on real pages.
    url: String,
    article_ids: Vec<ArticleId>,
    /// Best social score among the host's articles; the queue's sort key.
    score: f64,
}

/// What one host's lookup produced, before anything is written.
struct Outcome {
    host: String,
    article_ids: Vec<ArticleId>,
    validated: Vec<Validated>,
    rejected: usize,
    /// False only for a transport failure: the host is left unmemoized so the
    /// next run tries again.
    checked: bool,
}

/// Run one discovery pass over `articles` (plan §4 step 3).
///
/// Best effort by construction: a failing host is logged and skipped, and only
/// a database failure produces an `Err`. Safe to call with no articles and no
/// subscriptions.
pub async fn run(
    db: &Db,
    miniflux: &MinifluxClient,
    http: &reqwest::Client,
    cfg: &DiscoveryConfig,
    articles: &[Article],
    subscribed: &HashMap<FeedId, FeedMeta>,
    now: Timestamp,
) -> Result<Summary> {
    let mut summary = Summary::default();

    let reconciled = reconcile_added(db, subscribed, now)
        .await
        .context("reconciling feed candidates against subscriptions")?;

    let subscribed_hosts: HashSet<String> = subscribed
        .values()
        .flat_map(|meta| [host_of(&meta.feed_url), host_of(&meta.site_url)])
        .flatten()
        .collect();
    let subscribed_urls: HashSet<String> = subscribed
        .values()
        .map(|meta| norm_url(&meta.feed_url))
        .collect();

    // Group the aggregator-only articles by host, keeping the best social score
    // as the queue's priority and the best-scoring article as the lookup URL.
    let mut by_host: BTreeMap<String, Lookup> = BTreeMap::new();
    let mut skipped_hosts: HashSet<String> = HashSet::new();
    for article in articles.iter().filter(|a| aggregator_only(a)) {
        let Some(host) = host_of(&article.canonical_url) else {
            continue;
        };
        if skipped(&host, &cfg.skip_hosts) {
            continue;
        }
        if subscribed_hosts.contains(&host) {
            skipped_hosts.insert(host);
            continue;
        }
        let score = article.social_score();
        let entry = by_host.entry(host.clone()).or_insert_with(|| Lookup {
            host,
            url: article.canonical_url.clone(),
            article_ids: Vec::new(),
            score: f64::MIN,
        });
        entry.article_ids.push(article.id);
        if score > entry.score {
            entry.score = score;
            entry.url = article.canonical_url.clone();
        }
    }
    summary.hosts_skipped_subscribed = skipped_hosts.len();

    // Hosts checked recently just collect links; the rest queue for a lookup.
    let hosts: Vec<String> = by_host.keys().cloned().collect();
    let memo = hosts_checked(db, &hosts)
        .await
        .context("loading the discovery host memo")?;
    let recheck_before = now
        .checked_sub(jiff::Span::new().hours(RECHECK_DAYS * 24))
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let mut queue: Vec<Lookup> = Vec::new();
    for (host, lookup) in by_host {
        match memo.get(&host) {
            Some((checked_at, _)) if *checked_at >= recheck_before => {
                summary.hosts_reused += 1;
                for candidate_id in candidates_for_host(db, &host).await? {
                    for article_id in &lookup.article_ids {
                        link_article(db, candidate_id, *article_id).await?;
                        summary.articles_linked += 1;
                    }
                }
            }
            _ => queue.push(lookup),
        }
    }
    queue.sort_by(|a, b| b.score.total_cmp(&a.score));
    queue.truncate(cfg.max_lookups_per_run);

    tracing::info!(
        queued = queue.len(),
        reused = summary.hosts_reused,
        subscribed_hosts = summary.hosts_skipped_subscribed,
        reconciled,
        "feed discovery: looking up hosts"
    );
    if queue.is_empty() {
        return Ok(summary);
    }

    // Network only: every write happens below, in one sequential pass.
    let outcomes: Vec<Outcome> = futures::stream::iter(queue)
        .map(|lookup| look_up(miniflux, http, &subscribed_urls, lookup))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    for outcome in outcomes {
        if !outcome.checked {
            continue;
        }
        summary.hosts_checked += 1;
        summary.results_rejected += outcome.rejected;
        let kept = dedupe_guesses(outcome.validated);
        for validated in &kept {
            let is_new = existing_candidate_id(db, &validated.url).await?.is_none();
            let id = upsert_candidate(
                db,
                &validated.url,
                &outcome.host,
                validated.title().as_deref(),
                now,
            )
            .await
            .context("recording a feed candidate")?;
            if is_new {
                summary.candidates_new += 1;
                tracing::info!(
                    candidate = id,
                    feed_url = %validated.url,
                    title = ?validated.title(),
                    "new feed candidate"
                );
            }
            for article_id in &outcome.article_ids {
                link_article(db, id, *article_id).await?;
                summary.articles_linked += 1;
            }
        }
        mark_host_checked(db, &outcome.host, kept.len() as i64, now).await?;
    }
    Ok(summary)
}

/// Discover and validate one host's feeds. Never fails: a bad host is an
/// outcome, not an error.
async fn look_up(
    miniflux: &MinifluxClient,
    http: &reqwest::Client,
    subscribed_urls: &HashSet<String>,
    lookup: Lookup,
) -> Outcome {
    let mut outcome = Outcome {
        host: lookup.host.clone(),
        article_ids: lookup.article_ids,
        validated: Vec::new(),
        rejected: 0,
        checked: true,
    };
    let results = match miniflux.discover(&lookup.url).await {
        Ok(results) => results,
        // A `fetcher: …` / `resource not found` response is Miniflux's verdict
        // on the site: nothing found, and nothing to retry this run.
        Err(error @ MinifluxError::Status { .. }) => {
            tracing::info!(host = %lookup.host, %error, "discover found nothing");
            return outcome;
        }
        // A transport failure (or a response we could not parse) says nothing
        // about the host: leave it unmemoized so the next run tries again.
        Err(error) => {
            tracing::warn!(host = %lookup.host, %error, "discover failed; host left unchecked");
            outcome.checked = false;
            return outcome;
        }
    };

    for result in results.into_iter().filter(keep_result) {
        if subscribed_urls.contains(&norm_url(&result.url)) {
            continue;
        }
        match fetch_head(http, &result.url).await {
            Ok(body) if sniff_feed(&body).is_some() => outcome.validated.push(Validated {
                url: result.url,
                raw_title: result.title,
                kind: result.kind,
                sniffed_title: feed_title(&body),
            }),
            Ok(_) => {
                outcome.rejected += 1;
                tracing::debug!(url = %result.url, "discover result is not a feed");
            }
            Err(error) => {
                outcome.rejected += 1;
                tracing::debug!(url = %result.url, %error, "could not validate discover result");
            }
        }
    }
    outcome
}

/// GET at most [`MAX_BODY_BYTES`] of `url`, so a stray 4 GB "feed" cannot eat
/// the run's memory.
async fn fetch_head(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut resp = http.get(url).timeout(FETCH_TIMEOUT).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("{status}");
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_BODY_BYTES {
            body.truncate(MAX_BODY_BYTES);
            break;
        }
    }
    Ok(body)
}

/// Aggregator-only articles first seen at or after `since`, hydrated with their
/// sources and social scores (the CLI backfill's input, plan §4 step 6).
pub async fn articles_since(db: &Db, since: Timestamp) -> Result<Vec<Article>> {
    let ids = db.article_ids_since(since).await?;
    let mut articles: Vec<Article> = db.get_articles(&ids).await?.into_values().collect();
    articles.sort_by_key(|a| a.id);
    Ok(articles)
}

// ---------------------------------------------------------------------------
// Ranking (plan §4 step 4)
// ---------------------------------------------------------------------------

/// The score a feed with no evidence at all lands on, on the 0–1 scale.
pub const PRIOR: f64 = 0.3;
/// Shrinkage strength: how many prior-valued articles the mean is padded with.
/// One 95-utility article should not outrank three 80s.
pub const K: f64 = 2.0;

/// What one linked article says about the feed behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct ArticleEvidence {
    pub article_id: ArticleId,
    pub title: String,
    /// −1…1: the explicit verdict if there is one, else how far the article got.
    pub value: f64,
    /// Interest names this article matched, from `signals_json`.
    pub interests: Vec<String>,
}

/// Shrunk mean of the evidence, 0–100 (plan §3 "Ranking").
pub fn score(evidence: &[ArticleEvidence]) -> f64 {
    let sum: f64 = evidence.iter().map(|e| e.value).sum();
    let mean = (sum + PRIOR * K) / (evidence.len() as f64 + K);
    (mean * 100.0).clamp(0.0, 100.0)
}

/// The three interests that come up most often across the linked articles.
pub fn why(evidence: &[ArticleEvidence]) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for name in evidence.iter().flat_map(|e| e.interests.iter()) {
        *counts.entry(name.as_str()).or_insert(0) += 1;
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    // Frequency first; the BTreeMap already has ties in name order.
    ranked.sort_by_key(|a| std::cmp::Reverse(a.1));
    ranked
        .into_iter()
        .take(3)
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Load the ranking evidence for `article_ids`: the newest `candidate_runs` row
/// per article, overridden by an explicit rating where one exists.
///
/// Articles the pipeline never scored and nobody rated simply have no entry;
/// they contribute nothing but the prior.
pub async fn load_evidence(
    db: &Db,
    article_ids: &[ArticleId],
    rating_lookback_days: i64,
) -> Result<HashMap<ArticleId, ArticleEvidence>> {
    let mut evidence: HashMap<ArticleId, ArticleEvidence> = HashMap::new();
    if article_ids.is_empty() {
        return Ok(evidence);
    }
    let wanted: HashSet<ArticleId> = article_ids.iter().copied().collect();
    let ids: Vec<ArticleId> = wanted.iter().copied().collect();

    let mut titles: HashMap<ArticleId, String> = HashMap::new();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, COALESCE(title, '') AS title FROM articles WHERE id IN ({placeholders})"
        )));
        for id in chunk {
            query = query.bind(*id);
        }
        for row in query.fetch_all(db.pool()).await? {
            titles.insert(row.get("id"), row.get("title"));
        }

        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT article_id, stage, utility, signals_json FROM candidate_runs
             WHERE article_id IN ({placeholders}) ORDER BY article_id, run_id DESC"
        )));
        for id in chunk {
            query = query.bind(*id);
        }
        for row in query.fetch_all(db.pool()).await? {
            let article_id: ArticleId = row.get("article_id");
            // Rows come newest-run first per article; keep the first.
            if evidence.contains_key(&article_id) {
                continue;
            }
            let signals: SignalsJson =
                serde_json::from_str(&row.get::<String, _>("signals_json")).unwrap_or_default();
            let stage: String = row.get("stage");
            let utility: Option<f64> = row.get("utility");
            let Some(value) = stage_value(&stage, utility, &signals) else {
                continue;
            };
            evidence.insert(
                article_id,
                ArticleEvidence {
                    article_id,
                    title: titles.get(&article_id).cloned().unwrap_or_default(),
                    value,
                    interests: signals
                        .top_interests
                        .iter()
                        .map(|i| i.name.clone())
                        .collect(),
                },
            );
        }
    }

    // An explicit verdict is the strongest evidence there is; it overrides.
    for rating in db.current_ratings(rating_lookback_days).await? {
        if !wanted.contains(&rating.article_id) {
            continue;
        }
        evidence
            .entry(rating.article_id)
            .or_insert_with(|| ArticleEvidence {
                article_id: rating.article_id,
                title: rating.title.clone(),
                value: 0.0,
                interests: Vec::new(),
            })
            .value = rating.value.clamp(-1.0, 1.0);
    }
    Ok(evidence)
}

/// How far the article got, on the −1…1 evidence scale.
fn stage_value(stage: &str, utility: Option<f64>, signals: &SignalsJson) -> Option<f64> {
    if stage == "selected" {
        return Some(0.85);
    }
    utility
        .or_else(|| signals.blend())
        .map(|value| (value / 100.0).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curate::signals::TopInterest;
    use crate::types::{ExtractMethod, SourceRef};

    async fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        (dir, db)
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A client pointed at `base_url`; the tests that pass one use an
    /// unroutable port, so any lookup at all would fail the test.
    fn miniflux(http: reqwest::Client, base_url: &str) -> MinifluxClient {
        MinifluxClient::new(
            &crate::config::MinifluxConfig {
                api_key: Some("token".into()),
                base_url: base_url.into(),
                ..Default::default()
            },
            http,
        )
        .unwrap()
    }

    fn discovered(title: &str, url: &str, kind: &str) -> Discovered {
        Discovered {
            url: url.into(),
            title: title.into(),
            kind: kind.into(),
        }
    }

    fn validated(result: &Discovered, sniffed_title: Option<&str>) -> Validated {
        Validated {
            url: result.url.clone(),
            raw_title: result.title.clone(),
            kind: result.kind.clone(),
            sniffed_title: sniffed_title.map(str::to_string),
        }
    }

    /// The two `<link rel=alternate>` hits observed for blog.philz.dev (plan §2).
    fn philz_results() -> Vec<Discovered> {
        vec![
            discovered(
                "blog.philz.dev",
                "https://blog.philz.dev/feed/feed.xml",
                "atom",
            ),
            discovered(
                "blog.philz.dev",
                "https://blog.philz.dev/feed/feed.json",
                "json",
            ),
        ]
    }

    /// The nine well-known-path guesses observed for zombo.com (plan §2).
    fn zombo_results() -> Vec<Discovered> {
        [
            ("https://zombo.com/atom.xml", "atom"),
            ("https://zombo.com/feed.atom", "atom"),
            ("https://zombo.com/feed.xml", "atom"),
            ("https://zombo.com/feed/", "atom"),
            ("https://zombo.com/index.rss", "rss"),
            ("https://zombo.com/index.xml", "rss"),
            ("https://zombo.com/rss.xml", "rss"),
            ("https://zombo.com/rss/", "rss"),
            ("https://zombo.com/rss/feed.xml", "rss"),
        ]
        .into_iter()
        .map(|(url, kind)| discovered(url, url, kind))
        .collect()
    }

    const SPA_SHELL: &[u8] = br#"<!doctype html><html><head><title>Zombo</title></head>
        <body><div id="root"></div><script src="/app.js"></script></body></html>"#;

    fn article(id: ArticleId, url: &str, kinds: &[SourceKind]) -> Article {
        Article {
            id,
            canonical_url: url.into(),
            title: format!("Article {id}"),
            best_entry_id: id,
            content_html: String::new(),
            word_count: 100,
            excerpt_only: false,
            image_count: 0,
            sources: kinds
                .iter()
                .map(|kind| SourceRef {
                    entry_id: id,
                    feed_id: 1,
                    feed_title: "Feed".into(),
                    category: None,
                    kind: *kind,
                })
                .collect(),
            first_seen: ts("2026-09-07T00:00:00Z"),
            url: url.into(),
            author: None,
            feed_id: 1,
            feed_title: "Feed".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: Vec::new(),
            social: Vec::new(),
            extract_method: ExtractMethod::Miniflux,
        }
    }

    #[test]
    fn only_articles_without_a_direct_feed_are_leads() {
        assert!(aggregator_only(&article(
            1,
            "https://example.com/a",
            &[SourceKind::HnFrontpage]
        )));
        assert!(aggregator_only(&article(2, "https://example.com/b", &[])));
        assert!(!aggregator_only(&article(
            3,
            "https://example.com/c",
            &[SourceKind::HnFrontpage, SourceKind::Feed]
        )));
    }

    #[test]
    fn hosts_are_normalized_and_matched_by_suffix() {
        assert_eq!(
            host_of("https://WWW.Example.com/post?x=1"),
            Some("example.com".into())
        );
        assert_eq!(
            host_of("http://blog.philz.dev/"),
            Some("blog.philz.dev".into())
        );
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("mailto:a@b.com"), None);

        assert!(skipped("reddit.com", &["reddit.com".to_string()]));
        assert!(skipped("old.reddit.com", &["reddit.com".to_string()]));
        assert!(!skipped("notreddit.com", &["reddit.com".to_string()]));
        assert!(!skipped("example.com", &[]));
    }

    #[test]
    fn comment_feeds_are_dropped_before_they_are_fetched() {
        assert!(keep_result(&discovered(
            "Blog",
            "https://a.dev/feed.xml",
            "atom"
        )));
        assert!(!keep_result(&discovered(
            "Blog",
            "https://a.dev/comments/feed/",
            "rss"
        )));
        assert!(!keep_result(&discovered(
            "Comments for Blog",
            "https://a.dev/feed2",
            "rss"
        )));
        assert!(!keep_result(&discovered("Blog", "", "rss")));
    }

    #[test]
    fn sniff_accepts_real_feeds_and_rejects_app_shells() {
        // BOM + XML declaration + leading whitespace before the root element.
        let mut rss = vec![0xEF, 0xBB, 0xBF];
        rss.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\n  <rss version=\"2.0\"><channel><title>A Blog</title></channel></rss>");
        assert_eq!(sniff_feed(&rss), Some(FeedKind::Rss));

        assert_eq!(
            sniff_feed(b"<feed xmlns=\"http://www.w3.org/2005/Atom\"><title>X</title></feed>"),
            Some(FeedKind::Atom)
        );
        assert_eq!(
            sniff_feed(b"<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">"),
            Some(FeedKind::Rdf)
        );
        assert_eq!(
            sniff_feed(
                br#"{"version": "https://jsonfeed.org/version/1.1", "title": "A Blog",
                     "items": []}"#
            ),
            Some(FeedKind::Json)
        );

        assert_eq!(sniff_feed(SPA_SHELL), None);
        assert_eq!(sniff_feed(b""), None);
        assert_eq!(sniff_feed(b"{\"items\": []}"), None);
        assert_eq!(sniff_feed(b"<?xml version=\"1.0\"?><html><body>hi"), None);
    }

    #[test]
    fn feed_title_is_unescaped_and_trimmed() {
        assert_eq!(
            feed_title(
                br#"<?xml version="1.0"?><rss><channel>
                    <title> Ben &amp; Dave&#39;s &lt;Weekly&gt; </title>"#
            )
            .as_deref(),
            Some("Ben & Dave's <Weekly>")
        );
        assert_eq!(
            feed_title(b"<feed><title type=\"text\"><![CDATA[ Phil's blog ]]></title>").as_deref(),
            Some("Phil's blog")
        );
        assert_eq!(feed_title(b"<rss><channel><link>x</link>"), None);
        assert_eq!(feed_title(b"<rss><title>  </title>"), None);
    }

    #[test]
    fn one_link_tag_hit_per_title_preferring_xml() {
        let results = philz_results();
        let kept = dedupe_guesses(results.iter().map(|r| validated(r, None)).collect());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url, "https://blog.philz.dev/feed/feed.xml");
        assert_eq!(kept[0].title().as_deref(), Some("blog.philz.dev"));

        // JSON first still yields the XML flavour.
        let mut reversed = results;
        reversed.reverse();
        let kept = dedupe_guesses(reversed.iter().map(|r| validated(r, None)).collect());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, "atom");
    }

    #[test]
    fn at_most_one_guess_survives_and_it_takes_the_sniffed_title() {
        let kept = dedupe_guesses(
            zombo_results()
                .iter()
                .map(|r| validated(r, Some("Zombo Feed")))
                .collect(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url, "https://zombo.com/atom.xml");
        assert_eq!(kept[0].title().as_deref(), Some("Zombo Feed"));

        // Three link-tag hits plus a guess still stop at MAX_PER_HOST.
        let mut many: Vec<Validated> = ["Main", "Rust tag", "Notes", "Extra"]
            .iter()
            .enumerate()
            .map(|(i, title)| {
                validated(
                    &discovered(title, &format!("https://a.dev/{i}.xml"), "atom"),
                    None,
                )
            })
            .collect();
        many.push(validated(
            &discovered("https://a.dev/feed/", "https://a.dev/feed/", "rss"),
            None,
        ));
        let kept = dedupe_guesses(many);
        assert_eq!(kept.len(), MAX_PER_HOST);
        assert!(kept.iter().all(|k| !k.is_guess()));
    }

    #[test]
    fn the_zombo_response_yields_no_candidates_because_nothing_sniffs() {
        // Every one of the nine guessed paths answers with the SPA shell.
        let survivors: Vec<Validated> = zombo_results()
            .iter()
            .filter(|r| keep_result(r) && sniff_feed(SPA_SHELL).is_some())
            .map(|r| validated(r, None))
            .collect();
        assert!(survivors.is_empty());
        assert!(dedupe_guesses(survivors).is_empty());
    }

    // --- ranking -------------------------------------------------------

    fn evidence(value: f64, interests: &[&str]) -> ArticleEvidence {
        ArticleEvidence {
            article_id: 1,
            title: "T".into(),
            value,
            interests: interests.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn no_evidence_scores_the_prior() {
        assert!((score(&[]) - PRIOR * 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_rating_dominates_and_shrinkage_favours_consistency() {
        let loved = score(&[evidence(1.0, &[])]);
        let disliked = score(&[evidence(-1.0, &[])]);
        assert!(loved > 50.0 && disliked < 5.0);

        let one_great = score(&[evidence(0.95, &[])]);
        let three_good = score(&[evidence(0.8, &[]), evidence(0.8, &[]), evidence(0.8, &[])]);
        assert!(three_good > one_great, "{three_good} vs {one_great}");
        assert!((0.0..=100.0).contains(&three_good));
    }

    #[test]
    fn why_names_the_modal_interests() {
        let names = why(&[
            evidence(0.5, &["rust", "databases"]),
            evidence(0.5, &["rust", "typography"]),
            evidence(0.5, &["rust", "databases", "urbanism"]),
        ]);
        assert_eq!(names[0], "rust");
        assert_eq!(names[1], "databases");
        assert_eq!(names.len(), 3);
        assert!(why(&[]).is_empty());
    }

    #[test]
    fn stage_value_prefers_verdict_then_selection_then_utility_then_blend() {
        let mut signals = SignalsJson {
            v: 1,
            ..Default::default()
        };
        signals.weights.insert("interest".into(), 1.0);
        signals.norm.insert("interest".into(), 0.4);
        signals.top_interests.push(TopInterest {
            name: "rust".into(),
            z: 1.0,
            cos: 0.5,
        });

        assert_eq!(stage_value("selected", None, &signals), Some(0.85));
        assert_eq!(stage_value("assessed", Some(72.0), &signals), Some(0.72));
        let blend = stage_value("triaged", None, &signals).unwrap();
        assert!((blend - 0.4).abs() < 1e-9, "{blend}");
        assert_eq!(stage_value("excluded", None, &SignalsJson::default()), None);
    }

    // --- database ------------------------------------------------------

    async fn seed_article(db: &Db, url: &str) -> ArticleId {
        db.upsert_article(&article(0, url, &[SourceKind::HnFrontpage]))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upsert_bumps_last_seen_and_never_undoes_a_decision() {
        let (_dir, db) = test_db().await;
        let first = ts("2026-09-01T00:00:00Z");
        let id = upsert_candidate(&db, "https://a.dev/feed.xml", "a.dev", None, first)
            .await
            .unwrap();

        set_status(&db, id, "dismissed", None, first).await.unwrap();
        let later = ts("2026-09-08T00:00:00Z");
        let again = upsert_candidate(
            &db,
            "https://a.dev/feed.xml",
            "a.dev",
            Some("A Blog"),
            later,
        )
        .await
        .unwrap();
        assert_eq!(again, id);

        let row = candidate(&db, id).await.unwrap().unwrap();
        assert_eq!(row.status, "dismissed");
        assert_eq!(row.first_seen, fmt_ts(first));
        assert_eq!(row.last_seen, fmt_ts(later));
        assert_eq!(row.title.as_deref(), Some("A Blog"));
        assert_eq!(count(&db, "dismissed").await.unwrap(), 1);
        assert_eq!(count(&db, "candidate").await.unwrap(), 0);
        // A dismissed host's candidates are not offered for re-linking.
        assert!(candidates_for_host(&db, "a.dev").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_host_memo_records_what_was_checked() {
        let (_dir, db) = test_db().await;
        let now = ts("2026-09-07T12:00:00Z");
        assert!(
            hosts_checked(&db, &["a.dev".into()])
                .await
                .unwrap()
                .is_empty()
        );
        mark_host_checked(&db, "a.dev", 2, now).await.unwrap();
        mark_host_checked(&db, "a.dev", 3, now).await.unwrap();
        let memo = hosts_checked(&db, &["a.dev".into(), "b.dev".into()])
            .await
            .unwrap();
        assert_eq!(memo.len(), 1);
        assert_eq!(memo["a.dev"], (now, 3));
    }

    #[tokio::test]
    async fn reconcile_marks_candidates_the_operator_already_subscribes_to() {
        let (_dir, db) = test_db().await;
        let now = ts("2026-09-07T12:00:00Z");
        let by_url = upsert_candidate(&db, "https://a.dev/feed.xml", "a.dev", None, now)
            .await
            .unwrap();
        let by_host = upsert_candidate(&db, "https://b.dev/atom.xml", "b.dev", None, now)
            .await
            .unwrap();
        let untouched = upsert_candidate(&db, "https://c.dev/feed.xml", "c.dev", None, now)
            .await
            .unwrap();

        let mut subscribed = HashMap::new();
        subscribed.insert(
            7,
            FeedMeta {
                id: 7,
                title: "A".into(),
                site_url: "https://a.dev".into(),
                feed_url: "https://a.dev/feed.xml/".into(),
                category: None,
            },
        );
        subscribed.insert(
            9,
            FeedMeta {
                id: 9,
                title: "B".into(),
                site_url: "https://www.b.dev/".into(),
                feed_url: "https://b.dev/other.xml".into(),
                category: None,
            },
        );
        assert_eq!(reconcile_added(&db, &subscribed, now).await.unwrap(), 2);

        assert_eq!(
            candidate(&db, by_url).await.unwrap().unwrap().status,
            "added"
        );
        let b = candidate(&db, by_host).await.unwrap().unwrap();
        assert_eq!(b.status, "added");
        assert_eq!(b.miniflux_feed_id, Some(9));
        assert_eq!(
            candidate(&db, untouched).await.unwrap().unwrap().status,
            "candidate"
        );
        // Nothing left to reconcile on a second pass.
        assert_eq!(reconcile_added(&db, &subscribed, now).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn listing_paginates_and_carries_the_linked_articles() {
        let (_dir, db) = test_db().await;
        let article_id = seed_article(&db, "https://a.dev/post").await;
        for n in 0..5 {
            let now = ts("2026-09-07T12:00:00Z")
                .checked_add(jiff::Span::new().minutes(n))
                .unwrap();
            let id = upsert_candidate(&db, &format!("https://a.dev/{n}.xml"), "a.dev", None, now)
                .await
                .unwrap();
            link_article(&db, id, article_id).await.unwrap();
            link_article(&db, id, article_id).await.unwrap();
        }
        let (first, total) = list(&db, "candidate", 1, 2).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].feed_url, "https://a.dev/4.xml");
        assert_eq!(first[0].article_ids, vec![article_id]);

        let (last, _) = list(&db, "candidate", 3, 2).await.unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].feed_url, "https://a.dev/0.xml");
        assert_eq!(all_candidates(&db).await.unwrap().len(), 5);
        assert!(list(&db, "added", 1, 2).await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn evidence_prefers_a_rating_over_the_run_telemetry() {
        let (_dir, db) = test_db().await;
        let rated = seed_article(&db, "https://a.dev/rated").await;
        let scored = seed_article(&db, "https://a.dev/scored").await;
        let unknown = seed_article(&db, "https://a.dev/unknown").await;
        let run_id = db
            .start_run("2026-09-07".parse().unwrap(), ts("2026-09-07T00:00:00Z"))
            .await
            .unwrap();
        for (article_id, stage, utility) in
            [(rated, "excluded", None), (scored, "selected", Some(90.0))]
        {
            sqlx::query(
                "INSERT INTO candidate_runs (run_id, article_id, stage, signals_json, utility)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(run_id)
            .bind(article_id)
            .bind(stage)
            .bind(r#"{"v":1,"top_interests":[{"name":"rust","z":1.0,"cos":0.5}]}"#)
            .bind(utility)
            .execute(db.pool())
            .await
            .unwrap();
        }
        crate::rate::record_explicit(
            &crate::config::Config::default(),
            &db,
            rated,
            Some(crate::types::Vote::Loved),
            "test",
            None,
            None,
        )
        .await
        .unwrap();

        let evidence = load_evidence(&db, &[rated, scored, unknown], 180)
            .await
            .unwrap();
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[&rated].value, 1.0);
        assert_eq!(evidence[&scored].value, 0.85);
        assert_eq!(evidence[&scored].interests, vec!["rust".to_string()]);
        assert_eq!(evidence[&scored].title, "Article 0");
        assert!(!evidence.contains_key(&unknown));
        assert!(load_evidence(&db, &[], 180).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_run_with_nothing_to_do_is_a_no_op() {
        let (_dir, db) = test_db().await;
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let client = miniflux(http.clone(), "http://127.0.0.1:1/");
        let summary = run(
            &db,
            &client,
            &http,
            &DiscoveryConfig::default(),
            &[],
            &HashMap::new(),
            ts("2026-09-07T12:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(summary, Summary::default());
        assert_eq!(
            summary.to_string(),
            "0 hosts checked, 0 reused, 0 already subscribed · 0 results rejected · 0 new candidates, 0 articles linked"
        );
    }

    #[tokio::test]
    async fn subscribed_and_skipped_hosts_are_never_looked_up() {
        let (_dir, db) = test_db().await;
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let client = miniflux(http.clone(), "http://127.0.0.1:1/");

        let mut subscribed = HashMap::new();
        subscribed.insert(
            7,
            FeedMeta {
                id: 7,
                title: "A".into(),
                site_url: "https://www.a.dev/".into(),
                feed_url: "https://a.dev/feed.xml".into(),
                category: None,
            },
        );
        let articles = vec![
            article(1, "https://a.dev/post", &[SourceKind::HnFrontpage]),
            article(
                2,
                "https://news.ycombinator.com/item?id=1",
                &[SourceKind::HnFrontpage],
            ),
            article(3, "https://b.dev/post", &[SourceKind::Feed]),
        ];
        let summary = run(
            &db,
            &client,
            &http,
            &DiscoveryConfig::default(),
            &articles,
            &subscribed,
            ts("2026-09-07T12:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(summary.hosts_skipped_subscribed, 1);
        assert_eq!(summary.hosts_checked, 0);
        assert_eq!(summary.candidates_new, 0);
    }

    #[tokio::test]
    async fn a_fresh_memo_links_new_articles_without_a_lookup() {
        let (_dir, db) = test_db().await;
        let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT).unwrap();
        let client = miniflux(http.clone(), "http://127.0.0.1:1/");

        let now = ts("2026-09-07T12:00:00Z");
        let article_id = seed_article(&db, "https://b.dev/post").await;
        let candidate_id = upsert_candidate(&db, "https://b.dev/feed.xml", "b.dev", None, now)
            .await
            .unwrap();
        mark_host_checked(&db, "b.dev", 1, now).await.unwrap();

        let mut lead = article(article_id, "https://b.dev/post", &[SourceKind::Lobsters]);
        lead.id = article_id;
        let summary = run(
            &db,
            &client,
            &http,
            &DiscoveryConfig::default(),
            &[lead],
            &HashMap::new(),
            now.checked_add(jiff::Span::new().hours(24)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(summary.hosts_reused, 1);
        assert_eq!(summary.hosts_checked, 0);
        assert_eq!(summary.articles_linked, 1);
        assert_eq!(
            candidate(&db, candidate_id)
                .await
                .unwrap()
                .unwrap()
                .article_ids,
            vec![article_id]
        );
    }
}
