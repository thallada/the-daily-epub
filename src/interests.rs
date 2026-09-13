//! Standing-interest storage and rating-derived weights.
//!
//! Interest queries stay here so the central database layer remains focused on
//! the pipeline's shared records.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use serde::Deserialize;
use sqlx::Row as _;

use crate::config::Config;
use crate::curate::llm::{LlmClient, Llms, provider_meters};
use crate::curate::profile;
use crate::curate::signals::TopInterest;
use crate::db::{Db, fmt_ts};
use crate::types::ArticleId;

/// One standing interest and its optional prompt category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interest {
    pub id: i64,
    pub name: String,
    pub category: Option<String>,
    pub created_at: String,
    pub categorized_at: Option<String>,
}

/// Result of adding a name whose uniqueness is case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Added(i64),
    Duplicate,
}

/// One stored article-to-interest match.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchRow {
    pub article_id: ArticleId,
    pub interest_id: i64,
    pub name: String,
    pub cos: f64,
    pub z: f64,
}

/// Rating credit accumulated for one interest.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub up: f64,
    pub down: f64,
    pub n: usize,
}

impl Rate {
    /// Beta smoothing keeps an unrated interest neutral.
    pub fn weight(&self) -> f64 {
        (self.up + 1.0) / (self.up + self.down + 2.0)
    }
}

/// Interest rates plus the number of ratings that could affect them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rates {
    pub by_interest: HashMap<i64, Rate>,
    pub attributable: usize,
}

const INTEREST_COLUMNS: &str = "id, name, category, created_at, categorized_at";

/// Dashboard article search for one interest name.
pub fn articles_href(name: &str) -> String {
    format!(
        "/dashboard/articles?interest={}",
        crate::web::encode_component(name)
    )
}

/// Parse an OPML export for the one-time interests importer.
pub fn parse_opml(raw: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for chunk in raw.split("text=\"").skip(1) {
        let Some((value, _)) = chunk.split_once('"') else {
            continue;
        };
        let name = xml_unescape(value).trim().to_string();
        if !name.is_empty() && seen.insert(name.to_lowercase()) {
            out.push(name);
        }
    }
    out
}

fn xml_unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn interest_from(row: &sqlx::sqlite::SqliteRow) -> Interest {
    Interest {
        id: row.get("id"),
        name: row.get("name"),
        category: row.get("category"),
        created_at: row.get("created_at"),
        categorized_at: row.get("categorized_at"),
    }
}

/// All interests, ordered case-insensitively by name.
pub async fn list(db: &Db) -> Result<Vec<Interest>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {INTEREST_COLUMNS} FROM interests ORDER BY name COLLATE NOCASE, name"
    )))
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(interest_from).collect())
}

/// Add one trimmed, non-empty name of at most 80 characters.
pub async fn add(
    db: &Db,
    name: &str,
    category: Option<&str>,
    now: Timestamp,
) -> Result<AddOutcome> {
    let name = name.trim();
    let len = name.chars().count();
    if !(1..=80).contains(&len) {
        bail!("interest name must be 1–80 characters");
    }

    let result = sqlx::query(
        "INSERT OR IGNORE INTO interests (name, category, created_at, categorized_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(category)
    .bind(fmt_ts(now))
    .bind(category.map(|_| fmt_ts(now)))
    .execute(db.pool())
    .await?;
    if result.rows_affected() == 0 {
        Ok(AddOutcome::Duplicate)
    } else {
        Ok(AddOutcome::Added(result.last_insert_rowid()))
    }
}

/// Set or clear an interest category and its categorization timestamp together.
pub async fn set_category(db: &Db, id: i64, category: Option<&str>, now: Timestamp) -> Result<()> {
    sqlx::query("UPDATE interests SET category = ?, categorized_at = ? WHERE id = ?")
        .bind(category)
        .bind(category.map(|_| fmt_ts(now)))
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Delete an interest, its match rows, and its name-keyed cached embedding.
pub async fn delete(db: &Db, id: i64) -> Result<()> {
    let mut tx = db.pool().begin().await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM interests WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM interests WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(name) = name {
        sqlx::query("DELETE FROM interest_embeddings WHERE interest = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// All names in the stable order used for embedding requests.
pub async fn names(db: &Db) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT name FROM interests ORDER BY name COLLATE NOCASE, name")
            .fetch_all(db.pool())
            .await?,
    )
}

/// Names grouped for the prompt, with uncategorized interests last.
pub async fn grouped(db: &Db) -> Result<Vec<(String, Vec<String>)>> {
    let mut by_category: HashMap<String, Vec<String>> = HashMap::new();
    let mut other = Vec::new();
    for interest in list(db).await? {
        if let Some(category) = interest.category {
            by_category.entry(category).or_default().push(interest.name);
        } else {
            other.push(interest.name);
        }
    }

    let mut groups: Vec<_> = by_category.into_iter().collect();
    groups.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    for (_, members) in &mut groups {
        members.sort_by(|left, right| {
            left.to_lowercase()
                .cmp(&right.to_lowercase())
                .then_with(|| left.cmp(right))
        });
    }
    if !other.is_empty() {
        groups.push(("Other standing interests".to_string(), other));
    }
    Ok(groups)
}

/// Interests awaiting the categorizer, ordered case-insensitively by name.
pub async fn uncategorized(db: &Db) -> Result<Vec<Interest>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {INTEREST_COLUMNS} FROM interests WHERE category IS NULL
         ORDER BY name COLLATE NOCASE, name"
    )))
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(interest_from).collect())
}

/// Upsert the current run's recorded top-interest matches in one transaction.
pub async fn replace_matches(
    db: &Db,
    run_id: Option<i64>,
    matches: &[(ArticleId, Vec<TopInterest>)],
    ids: &HashMap<String, i64>,
) -> Result<()> {
    write_matches(db, run_id, matches, ids, true).await?;
    Ok(())
}

/// Insert backfilled matches without disturbing rows a real run wrote.
pub async fn insert_matches_if_absent(
    db: &Db,
    matches: &[(ArticleId, Vec<TopInterest>)],
    ids: &HashMap<String, i64>,
) -> Result<u64> {
    write_matches(db, None, matches, ids, false).await
}

/// One transaction over the top interests of every article, either upserting
/// (a run's own rows) or leaving whatever is already stored alone (a backfill).
async fn write_matches(
    db: &Db,
    run_id: Option<i64>,
    matches: &[(ArticleId, Vec<TopInterest>)],
    ids: &HashMap<String, i64>,
    replace: bool,
) -> Result<u64> {
    let sql = if replace {
        "INSERT INTO article_interests (article_id, interest_id, cos, z, run_id)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(article_id, interest_id) DO UPDATE SET
             cos = excluded.cos, z = excluded.z, run_id = excluded.run_id"
    } else {
        "INSERT OR IGNORE INTO article_interests (article_id, interest_id, cos, z, run_id)
         VALUES (?, ?, ?, ?, ?)"
    };
    let mut tx = db.pool().begin().await?;
    let mut written = 0;
    for (article_id, top_interests) in matches {
        for top in top_interests {
            let Some(interest_id) = ids.get(&top.name) else {
                continue;
            };
            written += sqlx::query(sql)
                .bind(article_id)
                .bind(interest_id)
                .bind(top.cos)
                .bind(top.z)
                .bind(run_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
    }
    tx.commit().await?;
    Ok(written)
}

const CATEGORIZE_PROMPT: &str = r#"TASK: file each new standing interest under one of the reader's interest categories.
Existing categories (reuse these names verbatim): {categories}
Create a new category only when none of the existing ones fits; a new category must be broad enough to hold several interests and named like the existing ones (two to five words, sentence case). Every interest gets exactly one category.
New interests: {interests}
Return JSON exactly: {"assignments":[{"interest":"…","category":"…"}]}"#;

#[derive(Debug, Deserialize)]
struct CategorizeResponse {
    #[serde(default)]
    assignments: Vec<CategoryAssignment>,
}

#[derive(Debug, Deserialize)]
struct CategoryAssignment {
    interest: String,
    category: String,
}

/// File every currently uncategorized interest in one bulk-model call.
pub async fn categorize(config: &Config, db: &Db) -> Result<String> {
    let pending = uncategorized(db).await?;
    if pending.is_empty() {
        return Ok("nothing to categorize".into());
    }
    let taste = profile::load_or_build(
        db,
        &config.profile_path,
        config.curation.feedback.verdicts_in_prompt,
    )
    .await?;
    let llms = Llms::from_config(config, taste.text, &provider_meters(config));
    let llm = llms
        .bulk
        .as_ref()
        .or_else(|| llms.editor_or_bulk())
        .context("no LLM provider is available for interest categorization")?;
    categorize_with_llm(db, &pending, llm).await
}

async fn categorize_with_llm(db: &Db, pending: &[Interest], llm: &LlmClient) -> Result<String> {
    let all = list(db).await?;
    let categories = all
        .iter()
        .filter_map(|interest| interest.category.as_deref())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let names = pending
        .iter()
        .map(|interest| interest.name.as_str())
        .collect::<Vec<_>>();
    let prompt = CATEGORIZE_PROMPT
        .replace("{categories}", &categories.join(", "))
        .replace("{interests}", &format!("\n{}", names.join("\n")));
    let response: CategorizeResponse = llm.complete_json(&prompt, 0.2).await?;

    let pending_by_name = pending
        .iter()
        .map(|interest| (interest.name.to_lowercase(), interest))
        .collect::<HashMap<_, _>>();
    let existing_categories = categories
        .iter()
        .map(|category| (category.to_lowercase(), *category))
        .collect::<HashMap<_, _>>();
    let mut assigned = BTreeSet::new();
    let mut new_categories = BTreeSet::new();
    let now = Timestamp::now();
    for assignment in response.assignments {
        let Some(interest) = pending_by_name.get(&assignment.interest.trim().to_lowercase()) else {
            continue;
        };
        let category = assignment.category.trim();
        if !(1..=60).contains(&category.chars().count()) || !assigned.insert(interest.id) {
            continue;
        }
        // A model that answers "software" for an existing "Software" must not
        // split the category.
        let category = match existing_categories.get(&category.to_lowercase()) {
            Some(existing) => existing,
            None => {
                new_categories.insert(category.to_string());
                category
            }
        };
        set_category(db, interest.id, Some(category), now).await?;
    }

    let mut message = format!(
        "categorized {} ({} new categories:",
        assigned.len(),
        new_categories.len()
    );
    if !new_categories.is_empty() {
        let _ = write!(
            message,
            " {}",
            new_categories.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    message.push(')');
    tracing::info!(%message);
    Ok(message)
}

/// Stored matches for the requested articles.
pub async fn matches_for_articles(db: &Db, article_ids: &[ArticleId]) -> Result<Vec<MatchRow>> {
    let mut matches = Vec::new();
    for chunk in article_ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT ai.article_id, ai.interest_id, i.name, ai.cos, ai.z
             FROM article_interests ai
             JOIN interests i ON i.id = ai.interest_id
             WHERE ai.article_id IN ({placeholders})"
        )));
        for article_id in chunk {
            query = query.bind(article_id);
        }
        for row in query.fetch_all(db.pool()).await? {
            matches.push(MatchRow {
                article_id: row.get("article_id"),
                interest_id: row.get("interest_id"),
                name: row.get("name"),
                cos: row.get("cos"),
                z: row.get("z"),
            });
        }
    }
    matches.sort_by(|left, right| {
        left.article_id
            .cmp(&right.article_id)
            .then_with(|| right.z.total_cmp(&left.z))
            .then_with(|| left.interest_id.cmp(&right.interest_id))
    });
    Ok(matches)
}

/// Number of stored article matches for each interest.
pub async fn match_counts(db: &Db) -> Result<HashMap<i64, i64>> {
    let rows = sqlx::query(
        "SELECT interest_id, COUNT(*) AS matches FROM article_interests GROUP BY interest_id",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get("interest_id"), row.get("matches")))
        .collect())
}

/// Derive smoothed interest rates from current ratings and their match rows.
pub fn rates(ratings: &[(ArticleId, f64, f64)], rows: &[(ArticleId, i64, f64)]) -> Rates {
    let mut rows_by_article: HashMap<ArticleId, Vec<(i64, f64)>> = HashMap::new();
    for &(article_id, interest_id, z) in rows {
        rows_by_article
            .entry(article_id)
            .or_default()
            .push((interest_id, z));
    }

    let mut result = Rates::default();
    for &(article_id, value, decay) in ratings {
        let mut attributed = false;
        if let Some(matches) = rows_by_article.get(&article_id) {
            for &(interest_id, z) in matches {
                let strength = (z / 3.0).clamp(0.0, 1.0);
                if strength <= 0.0 {
                    continue;
                }
                attributed = true;
                let credit = value * decay * strength;
                let rate = result.by_interest.entry(interest_id).or_default();
                rate.up += credit.max(0.0);
                rate.down += (-credit).max(0.0);
                rate.n += 1;
            }
        }
        if attributed {
            result.attributable += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(value: &str) -> Timestamp {
        value.parse().unwrap()
    }

    async fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        (dir, db)
    }

    #[test]
    fn opml_parser_unescapes_trims_and_deduplicates_names() {
        let interests = parse_opml(
            r#"<opml><body>
                <outline text=" Rust "/>
                <outline text="E-Ink &amp; RSS"/>
                <outline text="rust"/>
                <outline text="Quotes &quot;and&quot; apostrophes &apos;x&apos; &#39;y&#39;"/>
                <outline text="Markup &lt;tag&gt;"/>
                <outline text=""/>
            </body></opml>"#,
        );
        assert_eq!(
            interests,
            [
                "Rust",
                "E-Ink & RSS",
                "Quotes \"and\" apostrophes 'x' 'y'",
                "Markup <tag>",
            ]
        );
    }

    async fn seed_article(db: &Db, id: ArticleId) {
        sqlx::query(
            "INSERT INTO articles (id, canonical_url, title, first_seen) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("https://example.com/{id}"))
        .bind(format!("Article {id}"))
        .bind("2026-09-12T00:00:00Z")
        .execute(db.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn add_trims_names_and_uniqueness_is_case_insensitive() {
        let (_dir, db) = test_db().await;
        let now = ts("2026-09-12T12:00:00Z");
        let AddOutcome::Added(id) = add(&db, "  Rust  ", Some("Software"), now).await.unwrap()
        else {
            panic!("first insert should succeed");
        };
        assert_eq!(
            add(&db, "rust", None, now).await.unwrap(),
            AddOutcome::Duplicate
        );
        assert!(add(&db, "   ", None, now).await.is_err());
        assert!(add(&db, &"x".repeat(81), None, now).await.is_err());

        let interests = list(&db).await.unwrap();
        assert_eq!(interests.len(), 1);
        assert_eq!(interests[0].id, id);
        assert_eq!(interests[0].name, "Rust");
        assert_eq!(interests[0].category.as_deref(), Some("Software"));
        assert_eq!(
            interests[0].categorized_at.as_deref(),
            Some("2026-09-12T12:00:00Z")
        );
    }

    #[test]
    fn rates_apply_value_decay_strength_and_negative_credit() {
        let ratings = [
            (1, 1.0, 1.0),
            (2, 0.35, 1.0),
            (3, 1.0, 0.5),
            (4, -1.0, 0.5),
            (5, -1.0, 1.0),
        ];
        let rows = [
            (1, 10, 3.0),
            (2, 10, 1.5),
            (3, 11, 3.0),
            (4, 10, 0.9),
            (5, 12, 0.0),
        ];
        let rates = rates(&ratings, &rows);

        assert_eq!(rates.attributable, 4);
        let ten = rates.by_interest[&10];
        assert!((ten.up - 1.175).abs() < 1e-12);
        assert!((ten.down - 0.15).abs() < 1e-12);
        assert_eq!(ten.n, 3);
        assert!((ten.weight() - 2.175 / 3.325).abs() < 1e-12);
        assert_eq!(
            rates.by_interest[&11],
            Rate {
                up: 0.5,
                down: 0.0,
                n: 1
            }
        );
        assert!(!rates.by_interest.contains_key(&12));
        assert_eq!(Rate::default().weight(), 0.5);
    }

    #[tokio::test]
    async fn delete_cascades_matches_and_removes_the_embedding() {
        let (_dir, db) = test_db().await;
        seed_article(&db, 1).await;
        let now = ts("2026-09-12T12:00:00Z");
        let AddOutcome::Added(id) = add(&db, "Rust", None, now).await.unwrap() else {
            unreachable!();
        };
        sqlx::query(
            "INSERT INTO interest_embeddings
             (interest, model, dimension, embedding, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("Rust")
        .bind("test")
        .bind(1_i64)
        .bind(vec![0_u8; 4])
        .bind("2026-09-12T12:00:00Z")
        .execute(db.pool())
        .await
        .unwrap();

        let ids = HashMap::from([("Rust".to_string(), id)]);
        replace_matches(
            &db,
            Some(7),
            &[(
                1,
                vec![
                    TopInterest {
                        name: "Rust".into(),
                        cos: 0.7,
                        z: 1.2,
                    },
                    TopInterest {
                        name: "Unknown".into(),
                        cos: 0.9,
                        z: 2.0,
                    },
                ],
            )],
            &ids,
        )
        .await
        .unwrap();
        replace_matches(
            &db,
            None,
            &[(
                1,
                vec![TopInterest {
                    name: "Rust".into(),
                    cos: 0.8,
                    z: 1.5,
                }],
            )],
            &ids,
        )
        .await
        .unwrap();

        assert_eq!(match_counts(&db).await.unwrap(), HashMap::from([(id, 1)]));
        assert_eq!(
            matches_for_articles(&db, &[1]).await.unwrap(),
            [MatchRow {
                article_id: 1,
                interest_id: id,
                name: "Rust".into(),
                cos: 0.8,
                z: 1.5
            }]
        );
        let run_id: Option<i64> = sqlx::query_scalar(
            "SELECT run_id FROM article_interests WHERE article_id = 1 AND interest_id = ?",
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(run_id, None);

        delete(&db, id).await.unwrap();
        let match_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM article_interests")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let embedding_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM interest_embeddings")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(match_rows, 0);
        assert_eq!(embedding_rows, 0);
    }

    #[tokio::test]
    async fn grouped_sorts_categories_and_puts_uncategorized_last() {
        let (_dir, db) = test_db().await;
        let now = ts("2026-09-12T12:00:00Z");
        add(&db, "zebra", Some("Animals"), now).await.unwrap();
        add(&db, "Alpaca", Some("Animals"), now).await.unwrap();
        let AddOutcome::Added(id) = add(&db, "rust", None, now).await.unwrap() else {
            unreachable!();
        };
        add(&db, "Baking", Some("cooking"), now).await.unwrap();

        assert_eq!(
            grouped(&db).await.unwrap(),
            [
                ("Animals".into(), vec!["Alpaca".into(), "zebra".into()]),
                ("cooking".into(), vec!["Baking".into()]),
                ("Other standing interests".into(), vec!["rust".into()]),
            ]
        );
        assert_eq!(uncategorized(&db).await.unwrap()[0].id, id);

        set_category(&db, id, Some("Software"), now).await.unwrap();
        assert!(uncategorized(&db).await.unwrap().is_empty());
        set_category(&db, id, None, now).await.unwrap();
        let rust = uncategorized(&db).await.unwrap().pop().unwrap();
        assert_eq!(rust.categorized_at, None);
        assert_eq!(
            names(&db).await.unwrap(),
            ["Alpaca", "Baking", "rust", "zebra"]
        );
    }

    #[tokio::test]
    async fn empty_article_lookup_is_a_no_op() {
        let (_dir, db) = test_db().await;
        assert!(matches_for_articles(&db, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn categorizer_short_circuits_without_uncategorized_interests() {
        let (_dir, db) = test_db().await;
        add(
            &db,
            "Databases",
            Some("Software"),
            ts("2026-09-12T12:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(
            categorize(&Config::default(), &db).await.unwrap(),
            "nothing to categorize"
        );
    }

    #[tokio::test]
    async fn categorizer_assigns_known_names_and_tracks_new_categories() {
        use std::sync::Arc;

        use crate::config::ProviderConfig;
        use crate::curate::llm::{MockBackend, UsageMeter};
        use crate::types::TokenUsage;

        let (_dir, db) = test_db().await;
        let now = ts("2026-09-12T12:00:00Z");
        add(&db, "Databases", Some("Software"), now).await.unwrap();
        add(&db, "Rust macros", None, now).await.unwrap();
        add(&db, "Wheel-thrown pottery", None, now).await.unwrap();
        let pending = uncategorized(&db).await.unwrap();

        let backend = Arc::new(MockBackend::new());
        backend.push(
            r#"{"assignments":[
                {"interest":"RUST MACROS","category":" software "},
                {"interest":"Wheel-thrown pottery","category":"Creative crafts"},
                {"interest":"Not in the batch","category":"Made up"}
            ]}"#,
            TokenUsage::default(),
        );
        let llm = LlmClient::with_backend(
            "mock",
            "taste prompt".into(),
            UsageMeter::for_provider(&ProviderConfig::deepseek()),
            backend.clone(),
        );

        let message = categorize_with_llm(&db, &pending, &llm).await.unwrap();
        assert_eq!(message, "categorized 2 (1 new categories: Creative crafts)");
        let stored = list(&db).await.unwrap();
        assert_eq!(
            stored
                .iter()
                .find(|interest| interest.name == "Rust macros")
                .and_then(|interest| interest.category.as_deref()),
            Some("Software")
        );
        assert_eq!(
            stored
                .iter()
                .find(|interest| interest.name == "Wheel-thrown pottery")
                .and_then(|interest| interest.category.as_deref()),
            Some("Creative crafts")
        );
        assert_eq!(backend.calls(), 1);
        let request = &backend.prompts()[0];
        assert_eq!(request.temperature, 0.2);
        assert!(request.json);
        assert!(
            request
                .user
                .contains("Existing categories (reuse these names verbatim): Software")
        );
        assert!(
            request
                .user
                .contains("New interests: \nRust macros\nWheel-thrown pottery")
        );
    }
}
