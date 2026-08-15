//! SQLite access layer (spec §3.13).
//!
//! Runtime queries only — no `sqlx::query!` macros (implementation notes §1).
//! Timestamps are stored as RFC3339 UTC strings and dates as `YYYY-MM-DD`
//! (implementation notes §2). Every write is an idempotent upsert so that
//! `generate --date X` can be re-run safely (implementation notes §12).

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::Date;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::types::{
    Article, ArticleId, Entry, EntryId, FeedId, FeedPrior, LlmScore, Pick, Rating, SocialRef,
    SocialSource, SourceRef, Vote,
};

/// Embedded migrations from `./migrations` (implementation notes §1).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// `kv` key holding the ingest watermark (§3.1).
pub const KV_WATERMARK: &str = "ingest_watermark";
/// `kv` key holding the current taste profile document (§3.6).
pub const KV_TASTE_PROFILE: &str = "taste_profile";
/// `kv` key holding the taste profile version/build time (§3.6).
pub const KV_PROFILE_VERSION: &str = "profile_version";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("could not create database directory {path}: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed value in column `{column}`: {value}")]
    Decode { column: &'static str, value: String },
}

type Result<T> = std::result::Result<T, DbError>;

/// Handle to the shared SQLite pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (creating if needed) the database at `path` with WAL + foreign keys on,
    /// creating parent directories first. Does **not** run migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(30));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        Ok(Self { pool })
    }

    /// Open and run all pending migrations — used by every subcommand (§2).
    pub async fn open_and_migrate(path: &Path) -> Result<Self> {
        let db = Self::open(path).await?;
        db.migrate().await?;
        Ok(db)
    }

    /// Run the embedded migrations (`daily-epub db migrate`).
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Escape hatch for modules that need bespoke queries.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    // -----------------------------------------------------------------
    // kv (§3.1 watermark, §3.6 taste profile)
    // -----------------------------------------------------------------

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM kv WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO kv (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Latest successful ingest watermark (§3.1). The lookback window overlaps it,
    /// so a missing watermark simply means "use the full lookback window".
    pub async fn get_watermark(&self) -> Result<Option<Timestamp>> {
        match self.kv_get(KV_WATERMARK).await? {
            Some(s) => Ok(Some(parse_ts("watermark", &s)?)),
            None => Ok(None),
        }
    }

    pub async fn set_watermark(&self, ts: Timestamp) -> Result<()> {
        self.kv_set(KV_WATERMARK, &fmt_ts(ts)).await
    }

    // -----------------------------------------------------------------
    // entries (§3.1)
    // -----------------------------------------------------------------

    /// Upsert one raw Miniflux entry, keyed by the Miniflux entry id (§3.1).
    pub async fn upsert_entry(&self, entry: &Entry) -> Result<()> {
        sqlx::query(
            "INSERT INTO entries (id, feed_id, feed_title, category, title, url, canonical_url,
                                  author, published_at, comments_url, raw_content, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 feed_id = excluded.feed_id,
                 feed_title = excluded.feed_title,
                 category = excluded.category,
                 title = excluded.title,
                 url = excluded.url,
                 canonical_url = excluded.canonical_url,
                 author = excluded.author,
                 published_at = excluded.published_at,
                 comments_url = excluded.comments_url,
                 raw_content = excluded.raw_content,
                 fetched_at = excluded.fetched_at",
        )
        .bind(entry.id)
        .bind(entry.feed_id)
        .bind(entry.feed_title.as_deref())
        .bind(entry.category.as_deref())
        .bind(&entry.title)
        .bind(&entry.url)
        .bind(entry.canonical_url.as_deref())
        .bind(entry.author.as_deref())
        .bind(entry.published_at.map(fmt_ts))
        .bind(entry.comments_url.as_deref())
        .bind(&entry.raw_content)
        .bind(fmt_ts(entry.fetched_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a batch inside one transaction. Returns the number of rows written.
    pub async fn upsert_entries(&self, entries: &[Entry]) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            sqlx::query(
                "INSERT INTO entries (id, feed_id, feed_title, category, title, url, canonical_url,
                                      author, published_at, comments_url, raw_content, fetched_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     feed_id = excluded.feed_id,
                     feed_title = excluded.feed_title,
                     category = excluded.category,
                     title = excluded.title,
                     url = excluded.url,
                     canonical_url = excluded.canonical_url,
                     author = excluded.author,
                     published_at = excluded.published_at,
                     comments_url = excluded.comments_url,
                     raw_content = excluded.raw_content,
                     fetched_at = excluded.fetched_at",
            )
            .bind(entry.id)
            .bind(entry.feed_id)
            .bind(entry.feed_title.as_deref())
            .bind(entry.category.as_deref())
            .bind(&entry.title)
            .bind(&entry.url)
            .bind(entry.canonical_url.as_deref())
            .bind(entry.author.as_deref())
            .bind(entry.published_at.map(fmt_ts))
            .bind(entry.comments_url.as_deref())
            .bind(&entry.raw_content)
            .bind(fmt_ts(entry.fetched_at))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(entries.len())
    }

    /// Entries published within `[since, until]`, newest first (§3.2 input).
    pub async fn entries_in_window(
        &self,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<Entry>> {
        let rows = sqlx::query(
            "SELECT id, feed_id, feed_title, category, title, url, canonical_url, author,
                    published_at, comments_url, raw_content, fetched_at
             FROM entries
             WHERE published_at IS NOT NULL AND published_at >= ? AND published_at <= ?
             ORDER BY published_at DESC",
        )
        .bind(fmt_ts(since))
        .bind(fmt_ts(until))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(entry_from_row).collect()
    }

    pub async fn get_entry(&self, id: EntryId) -> Result<Option<Entry>> {
        let row = sqlx::query(
            "SELECT id, feed_id, feed_title, category, title, url, canonical_url, author,
                    published_at, comments_url, raw_content, fetched_at
             FROM entries WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(entry_from_row).transpose()
    }

    pub async fn count_entries(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM entries")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    // -----------------------------------------------------------------
    // articles (§3.2, §3.3)
    // -----------------------------------------------------------------

    /// Upsert a deduped cluster by canonical URL; returns its `articles.id`.
    ///
    /// The denormalized fields on [`Article`] are not stored here — they come
    /// from the joined `entries` row when loading.
    pub async fn upsert_article(&self, article: &Article) -> Result<ArticleId> {
        let sources = serde_json::to_string(&article.sources).unwrap_or_else(|_| "[]".into());
        let row = sqlx::query(
            "INSERT INTO articles (canonical_url, title, best_entry_id, content_html, word_count,
                                   excerpt_only, image_count, sources_json, first_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(canonical_url) DO UPDATE SET
                 title = excluded.title,
                 best_entry_id = excluded.best_entry_id,
                 content_html = excluded.content_html,
                 word_count = excluded.word_count,
                 excerpt_only = excluded.excerpt_only,
                 image_count = excluded.image_count,
                 sources_json = excluded.sources_json
             RETURNING id",
        )
        .bind(&article.canonical_url)
        .bind(&article.title)
        .bind(article.best_entry_id)
        .bind(&article.content_html)
        .bind(article.word_count)
        .bind(article.excerpt_only)
        .bind(article.image_count)
        .bind(sources)
        .bind(fmt_ts(article.first_seen))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    /// Load one article with its social refs and entry-derived fields.
    pub async fn get_article(&self, id: ArticleId) -> Result<Option<Article>> {
        let row = sqlx::query(ARTICLE_SELECT_BY_ID)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let mut article = article_from_row(&row)?;
        article.social = self.social_for_article(article.id).await?;
        Ok(Some(article))
    }

    pub async fn article_id_for_url(&self, canonical_url: &str) -> Result<Option<ArticleId>> {
        let row = sqlx::query("SELECT id FROM articles WHERE canonical_url = ?")
            .bind(canonical_url)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>("id")))
    }

    /// Article ids already published in some issue — never repeat them (§3.5).
    pub async fn previously_published_ids(&self) -> Result<Vec<ArticleId>> {
        let rows = sqlx::query("SELECT DISTINCT article_id FROM issue_articles")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<i64, _>("article_id")).collect())
    }

    /// Articles the LLM scored below `threshold` within the last `days` (§3.5).
    pub async fn recently_low_scored_ids(
        &self,
        threshold: f64,
        since: Date,
    ) -> Result<Vec<ArticleId>> {
        let rows = sqlx::query(
            "SELECT DISTINCT article_id FROM scores
             WHERE llm_score IS NOT NULL AND llm_score < ? AND run_date >= ?",
        )
        .bind(threshold)
        .bind(since.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<i64, _>("article_id")).collect())
    }

    // -----------------------------------------------------------------
    // social (§3.4)
    // -----------------------------------------------------------------

    pub async fn upsert_social(&self, r: &SocialRef) -> Result<()> {
        sqlx::query(
            "INSERT INTO social (article_id, source, item_id, score, num_comments, item_url, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(article_id, source) DO UPDATE SET
                 item_id = excluded.item_id,
                 score = excluded.score,
                 num_comments = excluded.num_comments,
                 item_url = excluded.item_url,
                 fetched_at = excluded.fetched_at",
        )
        .bind(r.article_id)
        .bind(r.source.as_str())
        .bind(r.item_id.as_deref())
        .bind(r.score)
        .bind(r.num_comments)
        .bind(r.item_url.as_deref())
        .bind(fmt_ts(r.fetched_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn social_for_article(&self, article_id: ArticleId) -> Result<Vec<SocialRef>> {
        let rows = sqlx::query(
            "SELECT article_id, source, item_id, score, num_comments, item_url, fetched_at
             FROM social WHERE article_id = ? ORDER BY source",
        )
        .bind(article_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(social_from_row).collect()
    }

    // -----------------------------------------------------------------
    // scores (§3.5, §3.6)
    // -----------------------------------------------------------------

    pub async fn upsert_score(
        &self,
        article_id: ArticleId,
        run_date: Date,
        prefilter_score: Option<f64>,
        llm: Option<&LlmScore>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO scores (article_id, run_date, prefilter_score, llm_score, llm_category, rationale)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(article_id, run_date) DO UPDATE SET
                 prefilter_score = COALESCE(excluded.prefilter_score, scores.prefilter_score),
                 llm_score = COALESCE(excluded.llm_score, scores.llm_score),
                 llm_category = COALESCE(excluded.llm_category, scores.llm_category),
                 rationale = COALESCE(excluded.rationale, scores.rationale)",
        )
        .bind(article_id)
        .bind(run_date.to_string())
        .bind(prefilter_score)
        .bind(llm.map(|l| l.score))
        .bind(llm.map(|l| l.category.as_str()))
        .bind(llm.map(|l| l.rationale.as_str()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // issues + lineup (§3.10)
    // -----------------------------------------------------------------

    /// Issue number = days since the first issue, 1-based (§3.10).
    pub async fn next_issue_number(&self, date: Date) -> Result<i64> {
        if let Some(row) = sqlx::query("SELECT issue_number FROM issues WHERE date = ?")
            .bind(date.to_string())
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(row.get::<i64, _>("issue_number"));
        }
        let row = sqlx::query("SELECT MIN(date) AS first_date FROM issues")
            .fetch_one(&self.pool)
            .await?;
        let first: Option<String> = row.get("first_date");
        match first {
            Some(s) => {
                let first = parse_date("issues.date", &s)?;
                Ok((date - first).get_days() as i64 + 1)
            }
            None => Ok(1),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_issue(
        &self,
        date: Date,
        issue_number: i64,
        generated_at: Timestamp,
        epub_path: Option<&str>,
        x4_path: Option<&str>,
        xtc_path: Option<&str>,
        front_page_html: Option<&str>,
        report_json: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at, epub_path, x4_path, xtc_path,
                                 front_page_html, report_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(date) DO UPDATE SET
                 issue_number = excluded.issue_number,
                 generated_at = excluded.generated_at,
                 epub_path = COALESCE(excluded.epub_path, issues.epub_path),
                 x4_path = COALESCE(excluded.x4_path, issues.x4_path),
                 xtc_path = COALESCE(excluded.xtc_path, issues.xtc_path),
                 front_page_html = COALESCE(excluded.front_page_html, issues.front_page_html),
                 report_json = COALESCE(excluded.report_json, issues.report_json)",
        )
        .bind(date.to_string())
        .bind(issue_number)
        .bind(fmt_ts(generated_at))
        .bind(epub_path)
        .bind(x4_path)
        .bind(xtc_path)
        .bind(front_page_html)
        .bind(report_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace the lineup for a date (regeneration is idempotent, notes §12).
    pub async fn replace_issue_articles(&self, date: Date, picks: &[Pick]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM issue_articles WHERE issue_date = ?")
            .bind(date.to_string())
            .execute(&mut *tx)
            .await?;
        for pick in picks {
            sqlx::query(
                "INSERT INTO issue_articles (issue_date, article_id, section, position, is_lead, summary)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(date.to_string())
            .bind(pick.article.id)
            .bind(&pick.section)
            .bind(pick.position)
            .bind(pick.is_lead)
            .bind(pick.summary.as_deref())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Recent issue dates + stored report JSON, newest first (`GET /issues.json`, §3.12).
    pub async fn recent_reports(&self, limit: i64) -> Result<Vec<(Date, Option<String>)>> {
        let rows = sqlx::query("SELECT date, report_json FROM issues ORDER BY date DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                let d = parse_date("issues.date", &r.get::<String, _>("date"))?;
                Ok((d, r.get::<Option<String>, _>("report_json")))
            })
            .collect()
    }

    /// Issues older than `cutoff`, used by retention pruning (§3.11).
    pub async fn issues_before(
        &self,
        cutoff: Date,
    ) -> Result<Vec<(Date, Option<String>, Option<String>, Option<String>)>> {
        let rows = sqlx::query(
            "SELECT date, epub_path, x4_path, xtc_path FROM issues WHERE date < ? ORDER BY date",
        )
        .bind(cutoff.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let d = parse_date("issues.date", &r.get::<String, _>("date"))?;
                Ok((
                    d,
                    r.get::<Option<String>, _>("epub_path"),
                    r.get::<Option<String>, _>("x4_path"),
                    r.get::<Option<String>, _>("xtc_path"),
                ))
            })
            .collect()
    }

    // -----------------------------------------------------------------
    // ratings + feed priors (§3.9)
    // -----------------------------------------------------------------

    /// Idempotent upsert of a reader vote (§3.9). Returns true if it changed anything.
    pub async fn upsert_rating(&self, rating: &Rating) -> Result<bool> {
        let res = sqlx::query(
            "INSERT INTO ratings (issue_date, article_id, vote, rated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(issue_date, article_id) DO UPDATE SET
                 vote = excluded.vote,
                 rated_at = excluded.rated_at
             WHERE ratings.vote != excluded.vote",
        )
        .bind(rating.issue_date.to_string())
        .bind(rating.article_id)
        .bind(rating.vote.as_i64())
        .bind(fmt_ts(rating.rated_at))
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Every rating joined to the feed that carried the article (§3.9 priors).
    pub async fn ratings_with_feed(&self) -> Result<Vec<(FeedId, Vote)>> {
        let rows = sqlx::query(
            "SELECT e.feed_id AS feed_id, r.vote AS vote
             FROM ratings r
             JOIN articles a ON a.id = r.article_id
             JOIN entries e ON e.id = a.best_entry_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let vote = if r.get::<i64, _>("vote") >= 0 {
                    Vote::Up
                } else {
                    Vote::Down
                };
                (r.get::<i64, _>("feed_id"), vote)
            })
            .collect())
    }

    /// Recent ratings with article titles, for the weekly profile rewrite (§3.6).
    pub async fn recent_ratings_detailed(&self, since: Date) -> Result<Vec<(Rating, String)>> {
        let rows = sqlx::query(
            "SELECT r.issue_date AS issue_date, r.article_id AS article_id, r.vote AS vote,
                    r.rated_at AS rated_at, a.title AS title
             FROM ratings r JOIN articles a ON a.id = r.article_id
             WHERE r.issue_date >= ? ORDER BY r.rated_at DESC",
        )
        .bind(since.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let rating = Rating {
                    issue_date: parse_date(
                        "ratings.issue_date",
                        &r.get::<String, _>("issue_date"),
                    )?,
                    article_id: r.get::<i64, _>("article_id"),
                    vote: if r.get::<i64, _>("vote") >= 0 {
                        Vote::Up
                    } else {
                        Vote::Down
                    },
                    rated_at: parse_ts("ratings.rated_at", &r.get::<String, _>("rated_at"))?,
                };
                Ok((rating, r.get::<String, _>("title")))
            })
            .collect()
    }

    pub async fn upsert_feed_prior(&self, prior: &FeedPrior) -> Result<()> {
        sqlx::query(
            "INSERT INTO feed_priors (feed_id, upvotes, downvotes, included)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(feed_id) DO UPDATE SET
                 upvotes = excluded.upvotes,
                 downvotes = excluded.downvotes,
                 included = excluded.included",
        )
        .bind(prior.feed_id)
        .bind(prior.upvotes)
        .bind(prior.downvotes)
        .bind(prior.included)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn feed_priors(&self) -> Result<Vec<FeedPrior>> {
        let rows = sqlx::query("SELECT feed_id, upvotes, downvotes, included FROM feed_priors")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| FeedPrior {
                feed_id: r.get::<i64, _>("feed_id"),
                upvotes: r.get::<i64, _>("upvotes"),
                downvotes: r.get::<i64, _>("downvotes"),
                included: r.get::<i64, _>("included"),
            })
            .collect())
    }

    // -----------------------------------------------------------------
    // runs (§3.6 cost guardrail, §3.13)
    // -----------------------------------------------------------------

    /// Insert a `running` row at the top of `generate`; returns `runs.id`.
    pub async fn start_run(&self, date: Date, started_at: Timestamp) -> Result<i64> {
        let row = sqlx::query(
            "INSERT INTO runs (date, started_at, status) VALUES (?, ?, 'running') RETURNING id",
        )
        .bind(date.to_string())
        .bind(fmt_ts(started_at))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    /// Write the final counters/status for a run (§3.13).
    pub async fn finish_run(&self, id: i64, report: &crate::report::RunReport) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET finished_at = ?, entries_fetched = ?, candidates = ?, selected = ?,
                             input_tokens = ?, cached_tokens = ?, output_tokens = ?, cost_usd = ?,
                             status = ?, error = ?
             WHERE id = ?",
        )
        .bind(report.finished_at.map(fmt_ts))
        .bind(report.counts.entries_fetched)
        .bind(report.counts.candidates)
        .bind(report.counts.selected)
        .bind(report.usage.input_tokens)
        .bind(report.usage.cached_tokens)
        .bind(report.usage.output_tokens)
        .bind(report.cost_usd)
        .bind(report.status.as_str())
        .bind(report.error.as_deref())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Total spend recorded for a date, for the `max_daily_usd` guardrail (§3.6).
    pub async fn spend_for_date(&self, date: Date) -> Result<f64> {
        let row =
            sqlx::query("SELECT COALESCE(SUM(cost_usd), 0.0) AS total FROM runs WHERE date = ?")
                .bind(date.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(row.get::<f64, _>("total"))
    }
}

/// Joined projection used by [`Db::get_article`]; keep in sync with [`article_from_row`].
const ARTICLE_SELECT_BY_ID: &str = "\
SELECT a.id AS id, a.canonical_url AS canonical_url, a.title AS title,
       a.best_entry_id AS best_entry_id, a.content_html AS content_html,
       a.word_count AS word_count, a.excerpt_only AS excerpt_only,
       a.image_count AS image_count, a.sources_json AS sources_json,
       a.first_seen AS first_seen,
       e.url AS entry_url, e.author AS author, e.feed_id AS feed_id,
       e.feed_title AS feed_title, e.category AS category,
       e.published_at AS published_at, e.comments_url AS comments_url
  FROM articles a LEFT JOIN entries e ON e.id = a.best_entry_id
 WHERE a.id = ?";

// ---------------------------------------------------------------------------
// Row mapping helpers (implementation notes §1: manual mapping, no macros)
// ---------------------------------------------------------------------------

/// RFC3339 UTC, the on-disk timestamp format (notes §2).
pub fn fmt_ts(ts: Timestamp) -> String {
    ts.to_string()
}

pub fn parse_ts(column: &'static str, value: &str) -> Result<Timestamp> {
    Timestamp::from_str(value).map_err(|_| DbError::Decode {
        column,
        value: value.to_string(),
    })
}

pub fn parse_date(column: &'static str, value: &str) -> Result<Date> {
    Date::from_str(value).map_err(|_| DbError::Decode {
        column,
        value: value.to_string(),
    })
}

fn entry_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Entry> {
    Ok(Entry {
        id: row.get("id"),
        feed_id: row.get("feed_id"),
        feed_title: row.get("feed_title"),
        category: row.get("category"),
        title: row.get("title"),
        url: row.get("url"),
        canonical_url: row.get("canonical_url"),
        author: row.get("author"),
        published_at: row
            .get::<Option<String>, _>("published_at")
            .map(|s| parse_ts("entries.published_at", &s))
            .transpose()?,
        comments_url: row.get("comments_url"),
        raw_content: row.get("raw_content"),
        fetched_at: parse_ts("entries.fetched_at", &row.get::<String, _>("fetched_at"))?,
    })
}

fn article_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Article> {
    let sources: Vec<SourceRef> =
        serde_json::from_str(&row.get::<String, _>("sources_json")).unwrap_or_default();
    Ok(Article {
        id: row.get("id"),
        canonical_url: row.get("canonical_url"),
        title: row.get::<Option<String>, _>("title").unwrap_or_default(),
        best_entry_id: row.get::<Option<i64>, _>("best_entry_id").unwrap_or(0),
        content_html: row
            .get::<Option<String>, _>("content_html")
            .unwrap_or_default(),
        word_count: row.get("word_count"),
        excerpt_only: row.get("excerpt_only"),
        image_count: row.get("image_count"),
        sources,
        first_seen: parse_ts("articles.first_seen", &row.get::<String, _>("first_seen"))?,
        url: row
            .get::<Option<String>, _>("entry_url")
            .unwrap_or_else(|| row.get("canonical_url")),
        author: row.get("author"),
        feed_id: row.get::<Option<i64>, _>("feed_id").unwrap_or(0),
        feed_title: row
            .get::<Option<String>, _>("feed_title")
            .unwrap_or_default(),
        category: row.get("category"),
        published_at: row
            .get::<Option<String>, _>("published_at")
            .map(|s| parse_ts("entries.published_at", &s))
            .transpose()?,
        comments_url: row.get("comments_url"),
        image_urls: Vec::new(),
        social: Vec::new(),
        extract_method: crate::types::ExtractMethod::Miniflux,
    })
}

fn social_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SocialRef> {
    let raw: String = row.get("source");
    let source = SocialSource::parse(&raw).ok_or(DbError::Decode {
        column: "social.source",
        value: raw,
    })?;
    Ok(SocialRef {
        article_id: row.get("article_id"),
        source,
        item_id: row.get("item_id"),
        score: row.get("score"),
        num_comments: row.get("num_comments"),
        item_url: row.get("item_url"),
        fetched_at: parse_ts("social.fetched_at", &row.get::<String, _>("fetched_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ExtractMethod;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn sample_entry(id: EntryId) -> Entry {
        Entry {
            id,
            feed_id: 7,
            feed_title: Some("Hacker News".into()),
            category: Some("Tech".into()),
            title: format!("Story {id}"),
            url: format!("https://example.com/{id}"),
            canonical_url: Some(format!("https://example.com/{id}")),
            author: Some("someone".into()),
            published_at: Some(ts("2026-08-15T04:00:00Z")),
            comments_url: Some("https://news.ycombinator.com/item?id=1".into()),
            raw_content: "<p>hello</p>".into(),
            fetched_at: ts("2026-08-15T05:30:00Z"),
        }
    }

    async fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("daily-epub.db");
        let db = Db::open_and_migrate(&path).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn open_creates_dirs_and_migrates() {
        let (dir, db) = temp_db().await;
        assert!(dir.path().join("nested").join("daily-epub.db").exists());
        // Migrations are idempotent.
        db.migrate().await.unwrap();
        assert_eq!(db.count_entries().await.unwrap(), 0);

        let row = sqlx::query("PRAGMA journal_mode")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>(0).to_lowercase(), "wal");
        let row = sqlx::query("PRAGMA foreign_keys")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>(0), 1);
    }

    #[tokio::test]
    async fn entry_upsert_is_idempotent() {
        let (_dir, db) = temp_db().await;
        db.upsert_entries(&[sample_entry(1), sample_entry(2)])
            .await
            .unwrap();
        db.upsert_entries(&[sample_entry(1)]).await.unwrap();
        assert_eq!(db.count_entries().await.unwrap(), 2);

        let mut changed = sample_entry(1);
        changed.title = "Renamed".into();
        db.upsert_entry(&changed).await.unwrap();
        let got = db.get_entry(1).await.unwrap().unwrap();
        assert_eq!(got.title, "Renamed");
        assert_eq!(got, changed);

        let window = db
            .entries_in_window(ts("2026-08-14T00:00:00Z"), ts("2026-08-16T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(window.len(), 2);
    }

    #[tokio::test]
    async fn kv_watermark_round_trip() {
        let (_dir, db) = temp_db().await;
        assert!(db.get_watermark().await.unwrap().is_none());
        let t = ts("2026-08-15T05:30:00Z");
        db.set_watermark(t).await.unwrap();
        assert_eq!(db.get_watermark().await.unwrap(), Some(t));
        db.kv_set("taste_profile", "hello").await.unwrap();
        assert_eq!(
            db.kv_get("taste_profile").await.unwrap().as_deref(),
            Some("hello")
        );
    }

    #[tokio::test]
    async fn article_and_social_round_trip() {
        let (_dir, db) = temp_db().await;
        db.upsert_entry(&sample_entry(1)).await.unwrap();
        let article = Article {
            id: 0,
            canonical_url: "https://example.com/1".into(),
            title: "Story 1".into(),
            best_entry_id: 1,
            content_html: "<p>body</p>".into(),
            word_count: 900,
            excerpt_only: false,
            image_count: 2,
            sources: vec![SourceRef {
                entry_id: 1,
                feed_id: 7,
                feed_title: "Hacker News".into(),
                category: Some("Tech".into()),
                kind: crate::types::SourceKind::HnFrontpage,
            }],
            first_seen: ts("2026-08-15T05:30:00Z"),
            url: "https://example.com/1".into(),
            author: None,
            feed_id: 7,
            feed_title: "Hacker News".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        };
        let id = db.upsert_article(&article).await.unwrap();
        assert_eq!(db.upsert_article(&article).await.unwrap(), id);

        db.upsert_social(&SocialRef {
            article_id: id,
            source: SocialSource::Hn,
            item_id: Some("42".into()),
            score: 342,
            num_comments: 210,
            item_url: Some("https://news.ycombinator.com/item?id=42".into()),
            fetched_at: ts("2026-08-15T05:31:00Z"),
        })
        .await
        .unwrap();

        let loaded = db.get_article(id).await.unwrap().unwrap();
        assert_eq!(loaded.word_count, 900);
        assert_eq!(loaded.sources.len(), 1);
        assert_eq!(loaded.social.len(), 1);
        assert_eq!(loaded.social[0].score, 342);
        assert_eq!(loaded.feed_title, "Hacker News");
        assert_eq!(
            db.article_id_for_url("https://example.com/1")
                .await
                .unwrap(),
            Some(id)
        );
    }

    #[tokio::test]
    async fn runs_and_issue_numbers() {
        let (_dir, db) = temp_db().await;
        let date: Date = "2026-08-15".parse().unwrap();
        assert_eq!(db.next_issue_number(date).await.unwrap(), 1);
        let run_id = db
            .start_run(date, ts("2026-08-15T05:30:00Z"))
            .await
            .unwrap();

        let mut report = crate::report::RunReport::new(date, ts("2026-08-15T05:30:00Z"));
        report.counts.entries_fetched = 412;
        report.counts.candidates = 120;
        report.counts.selected = 20;
        report.finish(ts("2026-08-15T05:36:00Z"), 0.14, 0.0028, 0.28);
        db.finish_run(run_id, &report).await.unwrap();

        db.upsert_issue(
            date,
            1,
            ts("2026-08-15T05:36:00Z"),
            None,
            None,
            None,
            None,
            Some("{}"),
        )
        .await
        .unwrap();
        let next: Date = "2026-08-16".parse().unwrap();
        assert_eq!(db.next_issue_number(next).await.unwrap(), 2);
        assert_eq!(db.recent_reports(5).await.unwrap().len(), 1);
        assert_eq!(db.spend_for_date(date).await.unwrap(), 0.0);
    }

    #[tokio::test]
    async fn ratings_upsert_is_idempotent() {
        let (_dir, db) = temp_db().await;
        let rating = Rating {
            issue_date: "2026-08-15".parse().unwrap(),
            article_id: 1,
            vote: Vote::Up,
            rated_at: ts("2026-08-15T12:00:00Z"),
        };
        assert!(db.upsert_rating(&rating).await.unwrap());
        assert!(!db.upsert_rating(&rating).await.unwrap());
        let flipped = Rating {
            vote: Vote::Down,
            ..rating.clone()
        };
        assert!(db.upsert_rating(&flipped).await.unwrap());
    }
}
