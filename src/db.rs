//! SQLite access layer (spec §3.13).
//!
//! Runtime queries only — no `sqlx::query!` macros (implementation notes §1).
//! Timestamps are stored as RFC3339 UTC strings and dates as `YYYY-MM-DD`
//! (implementation notes §2). Pipeline writes are idempotent upserts so that
//! `generate --date X` can be re-run safely; feedback events are append-only.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::Date;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::types::{
    Article, ArticleId, Entry, EntryId, Facets, Pick, RatedArticle, RatingEvent, SocialRef,
    SocialSource, SourceRef,
};

/// Embedded migrations from `./migrations` (implementation notes §1).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// `kv` key holding the ingest watermark (§3.1).
pub const KV_WATERMARK: &str = "ingest_watermark";
/// `kv` key holding the current system-prompt profile document (§8.4).
pub const KV_TASTE_PROFILE: &str = "taste_profile";
/// `kv` key holding the profile version/build time (§8.2).
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
    #[error("malformed JSON in column `{column}`: {source}")]
    Json {
        column: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

type Result<T> = std::result::Result<T, DbError>;

/// Handle to the shared SQLite pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct IssueRow {
    pub date: Date,
    pub issue_number: i64,
    pub generated_at: Timestamp,
    pub epub_path: Option<String>,
    pub x4_path: Option<String>,
    pub xtc_path: Option<String>,
    pub front_page_html: Option<String>,
    pub report_json: Option<String>,
    pub issue_json: Option<String>,
    pub bookorbit_book_id: Option<i64>,
    pub bookorbit_file_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct IssueListRow {
    pub date: Date,
    pub issue_number: i64,
    pub generated_at: Timestamp,
    pub article_count: i64,
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
    // kv (§3.1 watermark, §8 profile)
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
    /// Most denormalized fields on [`Article`] come from the joined `entries`
    /// row when loading; the extracted author is stored on `articles`.
    pub async fn upsert_article(&self, article: &Article) -> Result<ArticleId> {
        let sources = serde_json::to_string(&article.sources).unwrap_or_else(|_| "[]".into());
        let row = sqlx::query(
            "INSERT INTO articles (canonical_url, title, author, best_entry_id, content_html,
                                   word_count, excerpt_only, image_count, sources_json, first_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(canonical_url) DO UPDATE SET
                 title = excluded.title,
                 author = excluded.author,
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
        .bind(&article.author)
        .bind((article.best_entry_id != 0).then_some(article.best_entry_id))
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

    /// Every article in `ids`, keyed by id, with its social refs attached:
    /// one statement for the articles and one for the social rows, however
    /// many ids there are. Ids without a row are simply absent. This is what
    /// a page that shows a whole issue should call; `get_article` in a loop
    /// costs two statements per pick, and the per-statement overhead, not the
    /// SQLite work, was most of that page's origin time.
    pub async fn get_articles(&self, ids: &[ArticleId]) -> Result<HashMap<ArticleId, Article>> {
        let mut articles = HashMap::with_capacity(ids.len());
        // SQLite's default bound-parameter ceiling is 32 766; stay well under.
        for chunk in ids.chunks(500) {
            // Only the placeholder count is interpolated; every value is bound,
            // which is what `AssertSqlSafe` asserts.
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
                "{ARTICLE_SELECT_BY_IDS} ({placeholders})"
            )));
            for id in chunk {
                query = query.bind(*id);
            }
            for row in query.fetch_all(&self.pool).await? {
                let article = article_from_row(&row)?;
                articles.insert(article.id, article);
            }
            let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT article_id, source, item_id, score, num_comments, item_url, fetched_at
                 FROM social WHERE article_id IN ({placeholders}) ORDER BY article_id, source"
            )));
            for id in chunk {
                query = query.bind(*id);
            }
            for row in query.fetch_all(&self.pool).await? {
                let social = social_from_row(&row)?;
                if let Some(article) = articles.get_mut(&social.article_id) {
                    article.social.push(social);
                }
            }
        }
        Ok(articles)
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

    /// Article ids published before this issue date; same-date regeneration is allowed (§8.1).
    pub async fn previously_published_ids_before(&self, date: Date) -> Result<Vec<ArticleId>> {
        let rows =
            sqlx::query("SELECT DISTINCT article_id FROM issue_articles WHERE issue_date < ?")
                .bind(date.to_string())
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .map(|row| row.get::<i64, _>("article_id"))
            .collect())
    }

    /// Published article ids first seen at or after `since` (`features backfill`).
    pub async fn published_article_ids_since(&self, since: Timestamp) -> Result<Vec<ArticleId>> {
        let rows = sqlx::query(
            "SELECT DISTINCT ia.article_id FROM issue_articles ia
             JOIN articles a ON a.id = ia.article_id
             WHERE a.first_seen >= ?
             ORDER BY ia.article_id",
        )
        .bind(fmt_ts(since))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| row.get::<i64, _>("article_id"))
            .collect())
    }

    /// Every article id first seen at or after `since` (`features backfill --all`).
    pub async fn article_ids_since(&self, since: Timestamp) -> Result<Vec<ArticleId>> {
        let rows = sqlx::query("SELECT id FROM articles WHERE first_seen >= ? ORDER BY id")
            .bind(fmt_ts(since))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get::<i64, _>("id")).collect())
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
        issue_json: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO issues (date, issue_number, generated_at, epub_path, x4_path, xtc_path,
                                 front_page_html, report_json, issue_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(date) DO UPDATE SET
                 issue_number = excluded.issue_number,
                 generated_at = excluded.generated_at,
                 epub_path = COALESCE(excluded.epub_path, issues.epub_path),
                 x4_path = COALESCE(excluded.x4_path, issues.x4_path),
                 xtc_path = COALESCE(excluded.xtc_path, issues.xtc_path),
                 front_page_html = COALESCE(excluded.front_page_html, issues.front_page_html),
                 report_json = COALESCE(excluded.report_json, issues.report_json),
                 issue_json = COALESCE(excluded.issue_json, issues.issue_json)",
        )
        .bind(date.to_string())
        .bind(issue_number)
        .bind(fmt_ts(generated_at))
        .bind(epub_path)
        .bind(x4_path)
        .bind(xtc_path)
        .bind(front_page_html)
        .bind(report_json)
        .bind(issue_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One stored issue, including its serialized full-issue snapshot.
    pub async fn issue_by_date(&self, date: Date) -> Result<Option<IssueRow>> {
        let row = sqlx::query(
            "SELECT date, issue_number, generated_at, epub_path, x4_path, xtc_path,
                    front_page_html, report_json, issue_json,
                    bookorbit_book_id, bookorbit_file_id
             FROM issues WHERE date = ?",
        )
        .bind(date.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(issue_from_row).transpose()
    }

    /// Store or clear the BookOrbit reader ids cached for an issue date.
    pub async fn set_bookorbit_ids(&self, date: Date, ids: Option<(i64, i64)>) -> Result<()> {
        let (book_id, file_id) = match ids {
            Some((book_id, file_id)) => (Some(book_id), Some(file_id)),
            None => (None, None),
        };
        sqlx::query(
            "UPDATE issues
             SET bookorbit_book_id = ?, bookorbit_file_id = ?
             WHERE date = ?",
        )
        .bind(book_id)
        .bind(file_id)
        .bind(date.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Issue archive rows, newest first. A non-positive limit means all rows.
    pub async fn issue_dates(&self, limit: Option<i64>) -> Result<Vec<IssueListRow>> {
        let rows = sqlx::query(
            "SELECT i.date, i.issue_number, i.generated_at, COUNT(ia.article_id) AS article_count
             FROM issues i LEFT JOIN issue_articles ia ON ia.issue_date = i.date
             GROUP BY i.date, i.issue_number, i.generated_at
             ORDER BY i.date DESC
             LIMIT CASE WHEN ? > 0 THEN ? ELSE -1 END",
        )
        .bind(limit.unwrap_or(-1))
        .bind(limit.unwrap_or(-1))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(issue_list_from_row).collect()
    }

    pub async fn latest_issue_date(&self) -> Result<Option<Date>> {
        let raw: Option<String> = sqlx::query_scalar("SELECT MAX(date) FROM issues")
            .fetch_one(&self.pool)
            .await?;
        raw.map(|value| parse_date("issues.date", &value))
            .transpose()
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
                "INSERT INTO issue_articles (issue_date, article_id, section, position, is_lead, summary, why)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(date.to_string())
            .bind(pick.article.id)
            .bind(&pick.section)
            .bind(pick.position)
            .bind(pick.is_lead)
            .bind(pick.summary.as_deref())
            .bind(pick.why.as_deref())
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
    // append-only rating events (§6.2)
    // -----------------------------------------------------------------

    /// Append one feedback event and return its database id.
    pub async fn append_rating_event(&self, event: &RatingEvent) -> Result<i64> {
        let row = sqlx::query(
            "INSERT INTO rating_events
                 (article_id, issue_date, kind, source, label, value, note, event_at, user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(event.article_id)
        .bind(event.issue_date.map(|date| date.to_string()))
        .bind(&event.kind)
        .bind(&event.source)
        .bind(&event.label)
        .bind(event.value)
        .bind(event.note.as_deref())
        .bind(fmt_ts(event.event_at))
        .bind(event.user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// Latest issue containing an article, used to attach CLI feedback when possible.
    pub async fn latest_issue_date_for_article(
        &self,
        article_id: ArticleId,
    ) -> Result<Option<Date>> {
        let row = sqlx::query(
            "SELECT issue_date FROM issue_articles
             WHERE article_id = ? ORDER BY issue_date DESC LIMIT 1",
        )
        .bind(article_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            parse_date(
                "issue_articles.issue_date",
                &row.get::<String, _>("issue_date"),
            )
        })
        .transpose()
    }

    /// Current explicit verdicts, newest first. A latest `cleared` event removes
    /// its article from this learned set (§6.2).
    pub async fn current_ratings(&self, lookback_days: i64) -> Result<Vec<RatedArticle>> {
        self.latest_explicit_ratings(lookback_days, false).await
    }

    /// Current explicit events including `cleared`, for the ratings CLI.
    pub async fn current_ratings_including_cleared(
        &self,
        lookback_days: i64,
    ) -> Result<Vec<RatedArticle>> {
        self.latest_explicit_ratings(lookback_days, true).await
    }

    /// Raw authors of every article whose current explicit verdict is `slop`,
    /// regardless of age: a reported author stays penalized until the verdict is
    /// changed or cleared (§9.3). Articles without an author contribute nothing.
    pub async fn slop_authors(&self) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            "WITH ranked AS (
                 SELECT re.article_id, re.label,
                        ROW_NUMBER() OVER (
                            PARTITION BY re.article_id
                            ORDER BY re.event_at DESC, re.id DESC
                        ) AS event_rank
                 FROM rating_events re
                 WHERE re.kind = 'explicit'
             )
             SELECT DISTINCT COALESCE(a.author, e.author) AS author
             FROM ranked r
             JOIN articles a ON a.id = r.article_id
             LEFT JOIN entries e ON e.id = a.best_entry_id
             WHERE r.event_rank = 1 AND r.label = 'slop'
               AND COALESCE(a.author, e.author) IS NOT NULL
             ORDER BY author",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn latest_explicit_ratings(
        &self,
        lookback_days: i64,
        include_cleared: bool,
    ) -> Result<Vec<RatedArticle>> {
        let since = Timestamp::now()
            .checked_sub(jiff::Span::new().hours(lookback_days.max(0).saturating_mul(24)))
            .unwrap_or(Timestamp::UNIX_EPOCH);
        let rows = sqlx::query(
            "WITH ranked AS (
                 SELECT re.*,
                        ROW_NUMBER() OVER (
                            PARTITION BY re.article_id
                            ORDER BY re.event_at DESC, re.id DESC
                        ) AS event_rank
                 FROM rating_events re
                 WHERE re.kind = 'explicit' AND re.event_at >= ?
             )
             SELECT r.article_id, r.user_id, r.issue_date, r.label, r.value, r.note, r.event_at,
                    COALESCE(a.title, '') AS title,
                    COALESCE(e.feed_title,
                             CASE WHEN a.best_entry_id IS NULL THEN 'Imported' ELSE '' END)
                        AS feed_title,
                    (SELECT ia.summary FROM issue_articles ia
                     WHERE ia.article_id = r.article_id
                     ORDER BY ia.issue_date DESC LIMIT 1) AS summary,
                    aa.facets_json AS facets_json
             FROM ranked r
             JOIN articles a ON a.id = r.article_id
             LEFT JOIN entries e ON e.id = a.best_entry_id
             LEFT JOIN article_assessments aa
                    ON aa.article_id = r.article_id AND aa.stage = 'deep'
             WHERE r.event_rank = 1 AND (? OR r.label != 'cleared')
             ORDER BY r.event_at DESC, r.id DESC",
        )
        .bind(fmt_ts(since))
        .bind(include_cleared)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(rated_article_from_row).collect()
    }

    // -----------------------------------------------------------------
    // runs (§3.13, plan §7.6)
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
                             status = ?, error = ?, provider_costs_json = ?, config_json = ?,
                             report_json = ?
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
        .bind(
            serde_json::to_string(&report.provider_costs).map_err(|source| DbError::Json {
                column: "runs.provider_costs_json",
                source,
            })?,
        )
        .bind(
            serde_json::to_string(&report.config_json).map_err(|source| DbError::Json {
                column: "runs.config_json",
                source,
            })?,
        )
        .bind(report.to_json())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Earlier provider spend on the UTC date containing this run's start (§5).
    pub async fn provider_spend_for_utc_day(
        &self,
        started_at: Timestamp,
    ) -> Result<BTreeMap<String, f64>> {
        let utc_date = started_at
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date()
            .to_string();
        let rows = sqlx::query(
            "SELECT provider_costs_json FROM runs
             WHERE substr(started_at, 1, 10) = ? AND started_at < ?
               AND provider_costs_json IS NOT NULL",
        )
        .bind(utc_date)
        .bind(fmt_ts(started_at))
        .fetch_all(&self.pool)
        .await?;
        let mut totals = BTreeMap::new();
        for row in rows {
            let raw = row.get::<String, _>("provider_costs_json");
            let providers: BTreeMap<String, crate::report::ProviderUsage> =
                serde_json::from_str(&raw).map_err(|source| DbError::Json {
                    column: "runs.provider_costs_json",
                    source,
                })?;
            for (provider, usage) in providers {
                *totals.entry(provider).or_insert(0.0) += usage.cost_usd;
            }
        }
        Ok(totals)
    }
}

/// Joined projection used by [`Db::get_article`]; keep in sync with [`article_from_row`].
/// The article projection `article_from_row` reads, with the `WHERE` clause
/// supplied by the caller so the single-id and the `IN (...)` lookups cannot
/// drift apart.
macro_rules! article_select {
    ($where:literal) => {
        concat!(
            "SELECT a.id AS id, a.canonical_url AS canonical_url, a.title AS title,
       a.best_entry_id AS best_entry_id, a.content_html AS content_html,
       a.word_count AS word_count, a.excerpt_only AS excerpt_only,
       a.image_count AS image_count, a.sources_json AS sources_json,
       a.first_seen AS first_seen,
       e.url AS entry_url, COALESCE(a.author, e.author) AS author, e.feed_id AS feed_id,
       e.feed_title AS feed_title, e.category AS category,
       e.published_at AS published_at, e.comments_url AS comments_url
  FROM articles a LEFT JOIN entries e ON e.id = a.best_entry_id
 ",
            $where
        )
    };
}

const ARTICLE_SELECT_BY_ID: &str = article_select!("WHERE a.id = ?");
/// Followed at runtime by a parenthesised placeholder list.
const ARTICLE_SELECT_BY_IDS: &str = article_select!("WHERE a.id IN");

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
    let best_entry_id = row.get::<Option<i64>, _>("best_entry_id").unwrap_or(0);
    Ok(Article {
        id: row.get("id"),
        canonical_url: row.get("canonical_url"),
        title: row.get::<Option<String>, _>("title").unwrap_or_default(),
        best_entry_id,
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
            .or_else(|| (best_entry_id == 0).then(|| "Imported".into()))
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

fn rated_article_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RatedArticle> {
    let issue_date = row
        .get::<Option<String>, _>("issue_date")
        .map(|raw| parse_date("rating_events.issue_date", &raw))
        .transpose()?;
    let facets = row.get::<Option<String>, _>("facets_json").and_then(|raw| {
        match serde_json::from_str::<Facets>(&raw) {
            Ok(facets) => Some(facets),
            Err(error) => {
                tracing::warn!(%error, "ignoring malformed assessment facets");
                None
            }
        }
    });
    Ok(RatedArticle {
        article_id: row.get("article_id"),
        user_id: row.get("user_id"),
        issue_date,
        title: row.get("title"),
        feed_title: row.get("feed_title"),
        summary: row.get("summary"),
        facets,
        note: row.get("note"),
        value: row.get("value"),
        label: row.get("label"),
        event_at: parse_ts("rating_events.event_at", &row.get::<String, _>("event_at"))?,
    })
}

fn issue_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<IssueRow> {
    Ok(IssueRow {
        date: parse_date("issues.date", &row.get::<String, _>("date"))?,
        issue_number: row.get("issue_number"),
        generated_at: parse_ts("issues.generated_at", &row.get::<String, _>("generated_at"))?,
        epub_path: row.get("epub_path"),
        x4_path: row.get("x4_path"),
        xtc_path: row.get("xtc_path"),
        front_page_html: row.get("front_page_html"),
        report_json: row.get("report_json"),
        issue_json: row.get("issue_json"),
        bookorbit_book_id: row.get("bookorbit_book_id"),
        bookorbit_file_id: row.get("bookorbit_file_id"),
    })
}

fn issue_list_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<IssueListRow> {
    Ok(IssueListRow {
        date: parse_date("issues.date", &row.get::<String, _>("date"))?,
        issue_number: row.get("issue_number"),
        generated_at: parse_ts("issues.generated_at", &row.get::<String, _>("generated_at"))?,
        article_count: row.get("article_count"),
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
        for table in [
            "users",
            "sessions",
            "config_changes",
            "profile_versions",
            "jobs",
            "rating_imports",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(exists, 1, "missing {table}");
        }
        let columns: Vec<String> = sqlx::query("PRAGMA table_info(rating_events)")
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.get("name"))
            .collect();
        assert!(columns.contains(&"user_id".to_string()));
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
            author: Some("Page Writer".into()),
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
        assert_eq!(loaded.author.as_deref(), Some("Page Writer"));
        // The batched lookup agrees with the single one and skips unknown ids.
        let batch = db.get_articles(&[id, 9_999]).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[&id], loaded);
        assert!(db.get_articles(&[]).await.unwrap().is_empty());

        sqlx::query("UPDATE articles SET author = NULL WHERE id = ?")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
        let fallback = db.get_article(id).await.unwrap().unwrap();
        assert_eq!(fallback.author.as_deref(), Some("someone"));
        assert_eq!(
            db.article_id_for_url("https://example.com/1")
                .await
                .unwrap(),
            Some(id)
        );
    }

    #[tokio::test]
    async fn article_without_an_entry_writes_null_and_loads() {
        let (_dir, db) = temp_db().await;
        let article = Article {
            id: 0,
            canonical_url: "https://example.com/imported".into(),
            title: "Imported article".into(),
            best_entry_id: 0,
            content_html: "<p>Imported body</p>".into(),
            word_count: 2,
            excerpt_only: false,
            image_count: 0,
            sources: Vec::new(),
            first_seen: ts("2026-09-06T00:00:00Z"),
            url: "https://example.com/imported".into(),
            author: None,
            feed_id: 0,
            feed_title: "Imported".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: Vec::new(),
            social: Vec::new(),
            extract_method: ExtractMethod::Readability,
        };
        let id = db.upsert_article(&article).await.unwrap();
        let stored_entry: Option<i64> =
            sqlx::query_scalar("SELECT best_entry_id FROM articles WHERE id = ?")
                .bind(id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(stored_entry, None);
        let loaded = db.get_article(id).await.unwrap().unwrap();
        assert_eq!(loaded.best_entry_id, 0);
        assert_eq!(loaded.feed_id, 0);
        assert_eq!(loaded.feed_title, "Imported");
        assert!(loaded.sources.is_empty());
        assert!(crate::curate::signals::direct_feeds(&loaded).is_empty());
        db.append_rating_event(&RatingEvent {
            id: 0,
            user_id: None,
            article_id: id,
            issue_date: None,
            kind: "explicit".into(),
            source: "import".into(),
            label: "loved".into(),
            value: 1.0,
            note: None,
            event_at: ts("2026-09-06T00:00:00Z"),
        })
        .await
        .unwrap();
        let ratings = db.current_ratings(36_500).await.unwrap();
        assert_eq!(ratings[0].feed_title, "Imported");
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
        report.finish(ts("2026-08-15T05:36:00Z"));
        db.finish_run(run_id, &report).await.unwrap();
        let stored_report: Option<String> =
            sqlx::query_scalar("SELECT report_json FROM runs WHERE id = ?")
                .bind(run_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(stored_report.as_deref(), Some(report.to_json().as_str()));

        db.upsert_issue(
            date,
            1,
            ts("2026-08-15T05:36:00Z"),
            None,
            None,
            None,
            None,
            Some("{}"),
            None,
        )
        .await
        .unwrap();
        let next: Date = "2026-08-16".parse().unwrap();
        assert_eq!(db.next_issue_number(next).await.unwrap(), 2);
        assert_eq!(db.recent_reports(5).await.unwrap().len(), 1);
        // No provider spend was recorded, so the budget-day preload is empty.
        let spend = db
            .provider_spend_for_utc_day(ts("2026-08-15T23:00:00Z"))
            .await
            .unwrap();
        assert!(spend.values().all(|usd| *usd == 0.0));
    }

    #[tokio::test]
    async fn bookorbit_ids_round_trip_and_clear() {
        let (_dir, db) = temp_db().await;
        let date: Date = "2026-08-15".parse().unwrap();
        db.upsert_issue(
            date,
            1,
            ts("2026-08-15T05:36:00Z"),
            Some("The Daily EPUB - 2026-08-15.epub"),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        db.set_bookorbit_ids(date, Some((42, 84))).await.unwrap();
        db.upsert_issue(
            date,
            1,
            ts("2026-08-15T05:37:00Z"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let issue = db.issue_by_date(date).await.unwrap().unwrap();
        assert_eq!(issue.bookorbit_book_id, Some(42));
        assert_eq!(issue.bookorbit_file_id, Some(84));

        db.set_bookorbit_ids(date, None).await.unwrap();
        let issue = db.issue_by_date(date).await.unwrap().unwrap();
        assert_eq!(issue.bookorbit_book_id, None);
        assert_eq!(issue.bookorbit_file_id, None);
    }

    async fn record_run(db: &Db, date: Date, started: &str, deepseek: f64, anthropic: f64) {
        use crate::report::{ProviderUsage, RunReport};
        let started_at = ts(started);
        let run_id = db.start_run(date, started_at).await.unwrap();
        let mut report = RunReport::new(date, started_at);
        report.provider_costs.insert(
            "deepseek".into(),
            ProviderUsage {
                usage: crate::types::TokenUsage {
                    input_tokens: 10,
                    ..Default::default()
                },
                cost_usd: deepseek,
            },
        );
        report.provider_costs.insert(
            "anthropic".into(),
            ProviderUsage {
                usage: crate::types::TokenUsage::default(),
                cost_usd: anthropic,
            },
        );
        report.config_json = serde_json::json!({"models": {"editor": "claude-opus-5"}});
        report.finish(started_at);
        db.finish_run(run_id, &report).await.unwrap();
    }

    #[tokio::test]
    async fn provider_spend_is_summed_by_the_utc_day_of_started_at() {
        let (_dir, db) = temp_db().await;
        let date: Date = "2026-08-15".parse().unwrap();
        record_run(&db, date, "2026-08-15T03:00:00Z", 0.10, 0.50).await;
        record_run(&db, date, "2026-08-15T23:30:00Z", 0.05, 0.0).await;
        // Nominal issue date 08-15 in New York, but already 08-16 in UTC: a
        // different budget day (§5).
        record_run(&db, date, "2026-08-16T01:00:00Z", 1.0, 1.0).await;

        let spend = db
            .provider_spend_for_utc_day(ts("2026-08-15T23:45:00Z"))
            .await
            .unwrap();
        assert!((spend["deepseek"] - 0.15).abs() < 1e-9);
        assert!((spend["anthropic"] - 0.5).abs() < 1e-9);
        // Only runs that started earlier than this one count.
        let spend = db
            .provider_spend_for_utc_day(ts("2026-08-15T12:00:00Z"))
            .await
            .unwrap();
        assert!((spend["deepseek"] - 0.10).abs() < 1e-9);
        let spend = db
            .provider_spend_for_utc_day(ts("2026-08-17T12:00:00Z"))
            .await
            .unwrap();
        assert!(spend.is_empty());

        // `provider_costs_json` and `config_json` were written and round-trip.
        let row =
            sqlx::query("SELECT provider_costs_json, config_json FROM runs ORDER BY id LIMIT 1")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let costs: BTreeMap<String, crate::report::ProviderUsage> =
            serde_json::from_str(&row.get::<String, _>("provider_costs_json")).unwrap();
        assert_eq!(costs["deepseek"].usage.input_tokens, 10);
        assert!((costs["anthropic"].cost_usd - 0.5).abs() < 1e-9);
        let config: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("config_json")).unwrap();
        assert_eq!(config["models"]["editor"], "claude-opus-5");
    }

    #[tokio::test]
    async fn latest_explicit_rating_wins_and_clear_removes_it() {
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
            image_count: 0,
            sources: vec![],
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
        let article_id = db.upsert_article(&article).await.unwrap();
        for (date, number, summary) in [
            ("2026-08-14", 1, "Older summary"),
            ("2026-08-15", 2, "Newest summary"),
        ] {
            db.upsert_issue(
                date.parse().unwrap(),
                number,
                ts("2026-08-15T05:30:00Z"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO issue_articles
                     (issue_date, article_id, section, position, is_lead, summary)
                 VALUES (?, ?, 'Top Stories', 1, 0, ?)",
            )
            .bind(date)
            .bind(article_id)
            .bind(summary)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO article_assessments
                 (article_id, stage, model, prompt_version, facets_json, assessed_at)
             VALUES (?, 'deep', 'mock', 1, ?, '2026-08-15T11:00:00Z')",
        )
        .bind(article_id)
        .bind(r#"{"format":"analysis_essay","depth":"deep","evidence":null,"commerciality":null,"topic_group":"software_engineering","technicality":"advanced","locality":null,"specific_topics":null}"#)
        .execute(db.pool())
        .await
        .unwrap();
        let event = |label: &str, value: f64, at: &str| RatingEvent {
            id: 0,
            user_id: None,
            article_id,
            issue_date: Some("2026-08-15".parse().unwrap()),
            kind: "explicit".into(),
            source: "cli".into(),
            label: label.into(),
            value,
            note: None,
            event_at: ts(at),
        };
        db.append_rating_event(&event("loved", 1.0, "2026-08-15T12:00:00Z"))
            .await
            .unwrap();
        db.append_rating_event(&event("good", 0.35, "2026-08-15T13:00:00Z"))
            .await
            .unwrap();
        let mut implicit = event("read_fully", 0.5, "2026-08-15T13:30:00Z");
        implicit.kind = "implicit".into();
        implicit.source = "bookorbit".into();
        db.append_rating_event(&implicit).await.unwrap();
        let ratings = db.current_ratings(36500).await.unwrap();
        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings[0].label, "good");
        assert_eq!(ratings[0].feed_title, "Hacker News");
        assert_eq!(ratings[0].summary.as_deref(), Some("Newest summary"));
        assert_eq!(
            ratings[0]
                .facets
                .as_ref()
                .and_then(|facets| facets.format.as_deref()),
            Some("analysis_essay")
        );

        db.append_rating_event(&event("cleared", 0.0, "2026-08-15T14:00:00Z"))
            .await
            .unwrap();
        assert!(db.current_ratings(36500).await.unwrap().is_empty());
        let events = db.current_ratings_including_cleared(36500).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "cleared");
        let sources: Vec<String> =
            sqlx::query_scalar("SELECT source FROM rating_events ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(sources, ["cli", "cli", "bookorbit", "cli"]);
    }

    #[tokio::test]
    async fn slop_authors_follow_the_latest_verdict_and_fall_back_to_the_entry_author() {
        let (_dir, db) = temp_db().await;
        db.upsert_entry(&sample_entry(1)).await.unwrap();
        db.upsert_entry(&sample_entry(2)).await.unwrap();
        let mut page_author = Article {
            id: 0,
            canonical_url: "https://example.com/1".into(),
            title: "Story 1".into(),
            best_entry_id: 1,
            content_html: "<p>body</p>".into(),
            word_count: 900,
            excerpt_only: false,
            image_count: 0,
            sources: vec![],
            first_seen: ts("2026-08-15T05:30:00Z"),
            url: "https://example.com/1".into(),
            author: Some("Page Writer".into()),
            feed_id: 7,
            feed_title: "Hacker News".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: vec![],
            social: vec![],
            extract_method: ExtractMethod::Miniflux,
        };
        let first = db.upsert_article(&page_author).await.unwrap();
        page_author.canonical_url = "https://example.com/2".into();
        page_author.url = "https://example.com/2".into();
        page_author.best_entry_id = 2;
        page_author.author = None;
        let second = db.upsert_article(&page_author).await.unwrap();
        let event = |article_id: i64, label: &str, at: &str| RatingEvent {
            id: 0,
            user_id: None,
            article_id,
            issue_date: None,
            kind: "explicit".into(),
            source: "cli".into(),
            label: label.into(),
            value: -1.0,
            note: None,
            event_at: ts(at),
        };
        assert!(db.slop_authors().await.unwrap().is_empty());
        // Ancient verdicts still count: there is no lookback.
        db.append_rating_event(&event(first, "slop", "2020-01-01T00:00:00Z"))
            .await
            .unwrap();
        db.append_rating_event(&event(second, "slop", "2026-08-15T12:00:00Z"))
            .await
            .unwrap();
        assert_eq!(db.slop_authors().await.unwrap(), ["Page Writer", "someone"]);
        // A later verdict on the same article replaces the slop one.
        db.append_rating_event(&event(second, "cleared", "2026-08-15T13:00:00Z"))
            .await
            .unwrap();
        assert_eq!(db.slop_authors().await.unwrap(), ["Page Writer"]);
    }

    #[tokio::test]
    async fn curation_v2_migration_copies_ratings_and_drops_old_tables() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES
             (1, 'https://example.com/loved', 'Loved', '2026-08-15T00:00:00Z'),
             (2, 'https://example.com/down', 'Down', '2026-08-15T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ratings (issue_date, article_id, vote, rated_at) VALUES
             ('2026-08-15', 1, 1, '2026-08-15T12:00:00Z'),
             ('2026-08-15', 2, -1, '2026-08-15T13:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!("../migrations/0002_curation_v2.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0003_drop_scores.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0004_web.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let rows = sqlx::query(
            "SELECT article_id, issue_date, kind, source, label, value, event_at, user_id
             FROM rating_events ORDER BY article_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<String, _>("label"), "loved");
        assert_eq!(rows[0].get::<f64, _>("value"), 1.0);
        assert_eq!(rows[1].get::<String, _>("label"), "not_for_me");
        assert_eq!(rows[1].get::<f64, _>("value"), -1.0);
        for row in &rows {
            assert_eq!(row.get::<String, _>("kind"), "explicit");
            assert_eq!(row.get::<String, _>("source"), "migration");
            assert_eq!(row.get::<String, _>("issue_date"), "2026-08-15");
            assert_eq!(row.get::<Option<i64>, _>("user_id"), None);
        }
        assert_eq!(rows[0].get::<String, _>("event_at"), "2026-08-15T12:00:00Z");

        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(!tables.iter().any(|table| table == "ratings"));
        assert!(!tables.iter().any(|table| table == "feed_priors"));
        assert!(!tables.iter().any(|table| table == "scores"));
        for expected in [
            "rating_events",
            "article_embeddings",
            "interest_embeddings",
            "article_assessments",
            "candidate_runs",
            "users",
            "sessions",
            "config_changes",
            "profile_versions",
            "jobs",
        ] {
            assert!(tables.iter().any(|table| table == expected), "{expected}");
        }
    }
}
