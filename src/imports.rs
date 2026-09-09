//! Historical rating imports queued from the dashboard.

use anyhow::{Context, Result};
use jiff::Timestamp;
use sqlx::Row as _;

use crate::config::Config;
use crate::curate::embedding::EmbeddingService;
use crate::db::{Db, fmt_ts};
use crate::extract::{Extractor, Page};
use crate::types::{Article, ExtractMethod, Extracted, Vote};

const BATCH_SIZE: i64 = 20;

/// One row in the historical rating import queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub id: i64,
    pub url: String,
    pub label: String,
    pub note: Option<String>,
    pub status: String,
    pub message: Option<String>,
    pub article_id: Option<i64>,
    pub requested_by: Option<i64>,
    pub requested_at: String,
    pub finished_at: Option<String>,
}

fn row_from(row: &sqlx::sqlite::SqliteRow) -> ImportRow {
    ImportRow {
        id: row.get("id"),
        url: row.get("url"),
        label: row.get("label"),
        note: row.get("note"),
        status: row.get("status"),
        message: row.get("message"),
        article_id: row.get("article_id"),
        requested_by: row.get("requested_by"),
        requested_at: row.get("requested_at"),
        finished_at: row.get("finished_at"),
    }
}

/// Insert one pending import row per URL and return how many were queued.
pub async fn queue(
    db: &Db,
    urls: &[String],
    label: &str,
    note: Option<&str>,
    requested_by: Option<i64>,
    now: Timestamp,
) -> Result<usize, sqlx::Error> {
    let mut tx = db.pool().begin().await?;
    let requested_at = fmt_ts(now);
    for url in urls {
        sqlx::query(
            "INSERT INTO rating_imports
                 (url, label, note, status, requested_by, requested_at)
             VALUES (?, ?, ?, 'pending', ?, ?)",
        )
        .bind(url)
        .bind(label)
        .bind(note)
        .bind(requested_by)
        .bind(&requested_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(urls.len())
}

/// Newest import rows for the ratings dashboard.
pub async fn recent(db: &Db, limit: i64) -> Result<Vec<ImportRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, url, label, note, status, message, article_id, requested_by,
                requested_at, finished_at
         FROM rating_imports ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(row_from).collect())
}

async fn pending(db: &Db) -> Result<Vec<ImportRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, url, label, note, status, message, article_id, requested_by,
                requested_at, finished_at
         FROM rating_imports WHERE status = 'pending' ORDER BY id LIMIT ?",
    )
    .bind(BATCH_SIZE)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(row_from).collect())
}

async fn finish(
    db: &Db,
    id: i64,
    status: &str,
    message: &str,
    article_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE rating_imports
         SET status = ?, message = ?, article_id = ?, finished_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(short_message(message))
    .bind(article_id)
    .bind(fmt_ts(Timestamp::now()))
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

fn short_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

fn vote(label: &str) -> Result<Vote> {
    match label {
        "loved" => Ok(Vote::Loved),
        "good" => Ok(Vote::Good),
        "not_for_me" => Ok(Vote::NotForMe),
        "slop" => Ok(Vote::Slop),
        other => anyhow::bail!("invalid rating label {other:?}"),
    }
}

struct Processed {
    article_id: i64,
    created: bool,
    embedded: bool,
    embedding_note: Option<String>,
}

fn imported_article(
    canonical_url: String,
    page: Page,
    extracted: Extracted,
    first_seen: Timestamp,
) -> Article {
    Article {
        id: 0,
        canonical_url,
        title: if page.title.trim().is_empty() {
            page.final_url.clone()
        } else {
            page.title
        },
        best_entry_id: 0,
        content_html: extracted.content_html,
        word_count: extracted.word_count,
        excerpt_only: extracted.excerpt_only,
        image_count: extracted.image_urls.len() as i64,
        sources: Vec::new(),
        first_seen,
        url: page.final_url,
        author: extracted.author,
        feed_id: 0,
        feed_title: "Imported".into(),
        category: None,
        published_at: None,
        comments_url: None,
        image_urls: extracted.image_urls,
        social: Vec::new(),
        extract_method: ExtractMethod::Readability,
    }
}

async fn process(
    config: &Config,
    db: &Db,
    extractor: &Extractor,
    embedding: Option<&EmbeddingService>,
    embedding_unavailable: Option<&str>,
    row: &ImportRow,
) -> Result<Processed> {
    let canonical = crate::dedupe::canonical_url(&row.url)
        .with_context(|| format!("invalid URL {:?}", row.url))?;
    let (article, created) = match db.article_id_for_url(&canonical).await? {
        Some(id) => (
            db.get_article(id)
                .await?
                .with_context(|| format!("article {id} disappeared"))?,
            false,
        ),
        None => {
            let page = extractor
                .fetch_readable(&canonical)
                .await
                .with_context(|| format!("fetching {canonical}"))?;
            let extracted = extractor.finish_readable(&canonical, &page);
            let mut article = imported_article(canonical, page, extracted, Timestamp::now());
            article.id = db.upsert_article(&article).await?;
            (article, true)
        }
    };

    let mut embedded = false;
    let mut embedding_note = embedding_unavailable.map(str::to_string);
    if let Some(service) = embedding {
        match service.articles(std::slice::from_ref(&article)).await {
            Ok(vectors) if vectors.contains_key(&article.id) => embedded = true,
            Ok(_) => {
                tracing::warn!(url = %row.url, article_id = article.id, "Voyage returned no embedding for rating import");
                embedding_note = Some("Voyage returned no embedding".into());
            }
            Err(error) => {
                tracing::warn!(url = %row.url, article_id = article.id, %error, "could not embed rating import");
                embedding_note = Some(error.to_string());
            }
        }
    } else if !config.voyage.enabled {
        embedding_note = Some("Voyage is disabled".into());
    }

    crate::rate::record_explicit(
        config,
        db,
        article.id,
        Some(vote(&row.label)?),
        "import",
        row.requested_by,
        row.note.clone(),
    )
    .await?;

    Ok(Processed {
        article_id: article.id,
        created,
        embedded,
        embedding_note,
    })
}

/// Fetch, embed and rate all pending historical imports.
pub async fn run(config: &Config, db: &Db) -> Result<String> {
    let http = crate::http::build_client(crate::http::DEFAULT_TIMEOUT)
        .context("building article HTTP client")?;
    let extractor = Extractor::new(http, config.curation.paywall_domains.clone());
    let (embedding, embedding_unavailable) = if config.voyage.enabled {
        match EmbeddingService::real(db.clone(), config.voyage.clone()) {
            Ok(service) => (Some(service), None),
            Err(error) => {
                tracing::warn!(%error, "Voyage embeddings unavailable for rating imports");
                (None, Some(error.to_string()))
            }
        }
    } else {
        (None, None)
    };
    run_with(
        config,
        db,
        &extractor,
        embedding.as_ref(),
        embedding_unavailable.as_deref(),
    )
    .await
}

async fn run_with(
    config: &Config,
    db: &Db,
    extractor: &Extractor,
    embedding: Option<&EmbeddingService>,
    embedding_unavailable: Option<&str>,
) -> Result<String> {
    let mut imported = 0usize;
    let mut created = 0usize;
    let mut existing = 0usize;
    let mut failed = 0usize;
    loop {
        let rows = pending(db).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            match process(
                config,
                db,
                extractor,
                embedding,
                embedding_unavailable,
                &row,
            )
            .await
            {
                Ok(done) => {
                    imported += 1;
                    if done.created {
                        created += 1;
                    } else {
                        existing += 1;
                    }
                    let mut message = if done.created {
                        "created".to_string()
                    } else {
                        "existing article".to_string()
                    };
                    if done.embedded {
                        message.push_str(" + embedded");
                    } else if let Some(note) = done.embedding_note {
                        message.push_str(&format!("; no embedding ({note})"));
                    }
                    finish(db, row.id, "ok", &message, Some(done.article_id)).await?;
                    tracing::info!(url = %row.url, article_id = done.article_id, %message, "rating import ok");
                }
                Err(error) => {
                    failed += 1;
                    let message = format!("{error:#}");
                    finish(db, row.id, "failed", &message, None).await?;
                    tracing::info!(url = %row.url, %message, "rating import failed");
                }
            }
        }
    }
    if imported == 0 && failed == 0 {
        Ok("no pending rating imports".into())
    } else {
        Ok(format!(
            "{imported} imported ({created} new, {existing} existing), {failed} failed"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Entry;

    async fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        (dir, db)
    }

    async fn existing_article(db: &Db, url: &str) -> i64 {
        let now: Timestamp = "2026-09-01T00:00:00Z".parse().unwrap();
        db.upsert_entry(&Entry {
            id: 42,
            feed_id: 7,
            feed_title: Some("Feed".into()),
            category: None,
            title: "Existing".into(),
            url: url.into(),
            canonical_url: Some(url.into()),
            author: None,
            published_at: None,
            comments_url: None,
            raw_content: "<p>Existing body</p>".into(),
            fetched_at: now,
        })
        .await
        .unwrap();
        db.upsert_article(&Article {
            id: 0,
            canonical_url: url.into(),
            title: "Existing".into(),
            best_entry_id: 42,
            content_html: "<p>Existing body</p>".into(),
            word_count: 2,
            excerpt_only: false,
            image_count: 0,
            sources: Vec::new(),
            first_seen: now,
            url: url.into(),
            author: None,
            feed_id: 7,
            feed_title: "Feed".into(),
            category: None,
            published_at: None,
            comments_url: None,
            image_urls: Vec::new(),
            social: Vec::new(),
            extract_method: ExtractMethod::Miniflux,
        })
        .await
        .unwrap()
    }

    fn config_without_voyage() -> Config {
        let mut config = Config::default();
        config.voyage.enabled = false;
        config
    }

    #[tokio::test]
    async fn existing_article_is_reused_and_rated() {
        let (_dir, db) = test_db().await;
        let id = existing_article(&db, "https://example.com/post").await;
        let user = crate::web::users::add(&db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        queue(
            &db,
            &["https://EXAMPLE.com/post/?utm_source=test".into()],
            "good",
            Some("still useful"),
            Some(user.id),
            Timestamp::now(),
        )
        .await
        .unwrap();
        let summary = run_with(
            &config_without_voyage(),
            &db,
            &Extractor::offline(Vec::new()),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary, "1 imported (0 new, 1 existing), 0 failed");
        assert_eq!(db.count_entries().await.unwrap(), 1);
        let current = db.current_ratings(36500).await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].article_id, id);
        assert_eq!(current[0].user_id, Some(user.id));
        assert_eq!(current[0].label, "good");
        assert_eq!(current[0].note.as_deref(), Some("still useful"));
        let source: String =
            sqlx::query_scalar("SELECT source FROM rating_events ORDER BY id DESC LIMIT 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(source, "import");
        let row = recent(&db, 1).await.unwrap().remove(0);
        assert_eq!(row.status, "ok");
        assert_eq!(row.article_id, Some(id));
        assert!(row.message.unwrap().contains("existing article"));
    }

    #[tokio::test]
    async fn invalid_url_fails_and_empty_run_is_benign() {
        let (_dir, db) = test_db().await;
        queue(
            &db,
            &["not-a-url".into()],
            "loved",
            None,
            None,
            Timestamp::now(),
        )
        .await
        .unwrap();
        let config = config_without_voyage();
        let extractor = Extractor::offline(Vec::new());
        assert_eq!(
            run_with(&config, &db, &extractor, None, None)
                .await
                .unwrap(),
            "0 imported (0 new, 0 existing), 1 failed"
        );
        let row = recent(&db, 1).await.unwrap().remove(0);
        assert_eq!(row.status, "failed");
        assert!(row.message.unwrap().contains("invalid URL"));
        assert_eq!(
            run_with(&config, &db, &extractor, None, None)
                .await
                .unwrap(),
            "no pending rating imports"
        );
    }

    #[tokio::test]
    async fn fetched_page_becomes_an_entryless_sanitized_article() {
        let (_dir, db) = test_db().await;
        let extractor = Extractor::offline(Vec::new());
        let page = Page {
            title: "A historical essay".into(),
            html: "<article><p>Useful old writing.</p><script>bad()</script><img src=\"/chart.png\"></article>".into(),
            author: Some("Essay Writer".into()),
            final_url: "https://example.com/essays/old".into(),
        };
        let extracted = extractor.finish_readable("https://example.com/old", &page);
        let mut article = imported_article(
            "https://example.com/old".into(),
            page,
            extracted,
            "2026-09-06T00:00:00Z".parse().unwrap(),
        );
        assert_eq!(article.title, "A historical essay");
        assert_eq!(article.best_entry_id, 0);
        assert_eq!(article.feed_title, "Imported");
        assert_eq!(article.author.as_deref(), Some("Essay Writer"));
        assert!(article.sources.is_empty());
        assert!(!article.content_html.contains("script"));
        assert_eq!(article.image_urls, ["https://example.com/chart.png"]);
        article.id = db.upsert_article(&article).await.unwrap();
        let loaded = db.get_article(article.id).await.unwrap().unwrap();
        assert_eq!(loaded.title, "A historical essay");
        assert_eq!(loaded.best_entry_id, 0);
        assert_eq!(loaded.feed_title, "Imported");
        assert_eq!(loaded.author.as_deref(), Some("Essay Writer"));
    }

    #[tokio::test]
    async fn rows_queued_while_running_are_picked_up_in_the_next_batch() {
        let (_dir, db) = test_db().await;
        existing_article(&db, "https://example.com/post").await;
        queue(
            &db,
            &["https://example.com/post".into()],
            "loved",
            None,
            None,
            Timestamp::now(),
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER queue_during_import AFTER UPDATE OF status ON rating_imports
             WHEN OLD.status = 'pending' AND NEW.status = 'ok' AND NEW.id = 1
             BEGIN
               INSERT INTO rating_imports
                   (url, label, status, requested_at)
               VALUES ('https://example.com/post', 'good', 'pending', '2026-09-01T00:00:00Z');
             END",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let summary = run_with(
            &config_without_voyage(),
            &db,
            &Extractor::offline(Vec::new()),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary, "2 imported (0 new, 2 existing), 0 failed");
        assert_eq!(recent(&db, 10).await.unwrap().len(), 2);
        let ratings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rating_events")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(ratings, 2);
    }
}
