use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Context;
use jiff::civil::Date;
use sqlx::Row;

use crate::db::Db;
use crate::pipeline::display_date;
use crate::types::{BehindThePaper, Colophon, Editorial, Issue, IssueMeta, Lineup, Pick};

#[derive(Debug, Clone)]
pub struct Download {
    pub label: String,
    pub href: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct IssueView {
    pub issue: Issue,
    pub downloads: Vec<Download>,
    pub from_json: bool,
}

pub async fn load(
    db: &Db,
    config: &crate::config::Config,
    date: Date,
) -> anyhow::Result<Option<IssueView>> {
    let Some(row) = db.issue_by_date(date).await? else {
        return Ok(None);
    };
    let (mut issue, from_json) = if let Some(raw) = row.issue_json.as_deref() {
        let mut issue: Issue = serde_json::from_str(raw).context("decoding issues.issue_json")?;
        for pick in &mut issue.lineup.picks {
            if let Some(article) = db.get_article(pick.article.id).await? {
                pick.article = article;
            }
        }
        (issue, true)
    } else {
        let rows = sqlx::query(
            "SELECT article_id, section, position, is_lead, summary, why
             FROM issue_articles WHERE issue_date = ? ORDER BY section, position",
        )
        .bind(date.to_string())
        .fetch_all(db.pool())
        .await?;
        let mut picks = Vec::with_capacity(rows.len());
        let mut seen_sections = Vec::new();
        let mut summaries = BTreeMap::new();
        for pick_row in rows {
            let article_id: i64 = pick_row.get("article_id");
            let Some(article) = db.get_article(article_id).await? else {
                continue;
            };
            let section: String = pick_row.get("section");
            if !seen_sections.contains(&section) {
                seen_sections.push(section.clone());
            }
            let summary: Option<String> = pick_row.get("summary");
            if let Some(summary) = &summary {
                summaries.insert(article_id, summary.clone());
            }
            picks.push(Pick {
                article,
                section,
                position: pick_row.get("position"),
                is_lead: pick_row.get("is_lead"),
                why: pick_row.get("why"),
                summary,
                llm: None,
                discussion: None,
            });
        }
        let configured: HashSet<&str> = config
            .curation
            .sections
            .iter()
            .map(String::as_str)
            .collect();
        let mut section_order: Vec<String> = config
            .curation
            .sections
            .iter()
            .filter(|section| seen_sections.contains(section))
            .cloned()
            .collect();
        section_order.extend(
            seen_sections
                .into_iter()
                .filter(|section| !configured.contains(section.as_str())),
        );
        picks.sort_by_key(|pick| {
            let section = section_order
                .iter()
                .position(|value| value == &pick.section)
                .unwrap_or(usize::MAX);
            (section, pick.position)
        });
        let total_words = picks.iter().map(|pick| pick.article.word_count).sum();
        let article_count = picks.len() as i64;
        let section_count = section_order.len() as i64;
        (
            Issue {
                meta: IssueMeta {
                    date,
                    issue_number: row.issue_number,
                    generated_at: row.generated_at,
                    display_date: display_date(date),
                    article_count,
                    section_count,
                    total_words,
                    reading_minutes: crate::types::reading_minutes(total_words),
                },
                lineup: Lineup {
                    date,
                    picks,
                    section_order,
                },
                editorial: Editorial {
                    front_page_html: row.front_page_html.unwrap_or_default(),
                    summaries,
                },
                world_briefing: None,
                colophon: Colophon::default(),
                behind: BehindThePaper::default(),
            },
            false,
        )
    };
    issue.meta.article_count = issue.lineup.picks.len() as i64;
    let downloads = [
        ("EPUB", row.epub_path.as_deref(), "epub"),
        ("X4 EPUB", row.x4_path.as_deref(), "epub"),
        ("XTC", row.xtc_path.as_deref(), "xtc"),
    ]
    .into_iter()
    .filter_map(|(label, raw, kind)| download(label, raw?, kind))
    .collect();
    Ok(Some(IssueView {
        issue,
        downloads,
        from_json,
    }))
}

fn download(label: &str, raw: &str, kind: &str) -> Option<Download> {
    let path = Path::new(raw);
    let metadata = path.metadata().ok()?;
    let name = path.file_name()?.to_str()?;
    Some(Download {
        label: label.to_string(),
        href: format!("/files/{kind}/{}", crate::web::encode_component(name)),
        size_bytes: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::types::{Entry, Issue};

    use super::*;

    async fn seeded_issue(with_json: bool) -> (tempfile::TempDir, Db, Issue) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_and_migrate(&dir.path().join("db.sqlite"))
            .await
            .unwrap();
        let mut issue = crate::epub::fixtures::issue();
        for pick in &mut issue.lineup.picks {
            let article = &pick.article;
            db.upsert_entry(&Entry {
                id: article.best_entry_id,
                feed_id: article.feed_id,
                feed_title: Some(article.feed_title.clone()),
                category: article.category.clone(),
                title: article.title.clone(),
                url: article.url.clone(),
                canonical_url: Some(article.canonical_url.clone()),
                author: article.author.clone(),
                published_at: article.published_at,
                comments_url: article.comments_url.clone(),
                raw_content: article.content_html.clone(),
                fetched_at: article.first_seen,
            })
            .await
            .unwrap();
            let id = db.upsert_article(article).await.unwrap();
            pick.article.id = id;
            for social in &mut pick.article.social {
                social.article_id = id;
                db.upsert_social(social).await.unwrap();
            }
        }
        let issue_json = with_json.then(|| {
            let mut snapshot = issue.clone();
            for pick in &mut snapshot.lineup.picks {
                pick.article.content_html.clear();
            }
            serde_json::to_string(&snapshot).unwrap()
        });
        db.upsert_issue(
            issue.meta.date,
            issue.meta.issue_number,
            issue.meta.generated_at,
            None,
            None,
            None,
            Some(&issue.editorial.front_page_html),
            Some("{\"status\":\"ok\"}"),
            issue_json.as_deref(),
        )
        .await
        .unwrap();
        db.replace_issue_articles(issue.meta.date, &issue.lineup.picks)
            .await
            .unwrap();
        (dir, db, issue)
    }

    #[tokio::test]
    async fn issue_json_loader_rehydrates_bodies_and_keeps_ephemeral_content() {
        let (_dir, db, source) = seeded_issue(true).await;
        let stored = db.issue_by_date(source.meta.date).await.unwrap().unwrap();
        let snapshot: Issue = serde_json::from_str(stored.issue_json.as_deref().unwrap()).unwrap();
        assert!(
            snapshot
                .lineup
                .picks
                .iter()
                .all(|pick| pick.article.content_html.is_empty())
        );
        let loaded = load(&db, &crate::config::Config::default(), source.meta.date)
            .await
            .unwrap()
            .unwrap();
        assert!(loaded.from_json);
        assert!(
            loaded
                .issue
                .lineup
                .picks
                .iter()
                .all(|pick| !pick.article.content_html.is_empty())
        );
        assert!(loaded.issue.world_briefing.is_some());
        assert!(loaded.issue.lineup.picks[0].discussion.is_some());
    }

    #[tokio::test]
    async fn fallback_loader_builds_reduced_issue_in_configured_section_order() {
        let (_dir, db, source) = seeded_issue(false).await;
        let loaded = load(&db, &crate::config::Config::default(), source.meta.date)
            .await
            .unwrap()
            .unwrap();
        assert!(!loaded.from_json);
        assert_eq!(
            loaded.issue.lineup.section_order,
            ["Top Stories", "Niche Corner"]
        );
        assert!(loaded.issue.world_briefing.is_none());
        assert!(
            loaded
                .issue
                .lineup
                .picks
                .iter()
                .all(|pick| pick.discussion.is_none())
        );
        assert!(loaded.issue.editorial.front_page_html.contains("coffee"));
    }

    #[tokio::test]
    async fn public_issue_archive_feed_robots_and_reports_are_served() {
        let (_dir, db, source) = seeded_issue(true).await;
        let app = crate::server::router(crate::server::AppState::new(
            db,
            crate::config::Config::default(),
            None,
        ));
        let issue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}", source.meta.date))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issue.status(), StatusCode::OK);
        assert_eq!(
            issue.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
        );
        let html = String::from_utf8(
            to_bytes(issue.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("The Lead Story"));
        assert!(html.contains("Hacker News"));
        assert!(!html.contains("Two stories today"));
        assert!(!html.contains("Something happened"));

        let archive = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/issues")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archive.status(), StatusCode::OK);

        let feed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/feed.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            feed.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/atom+xml; charset=utf-8"
        );
        let feed = String::from_utf8(
            to_bytes(feed.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let document = roxmltree::Document::parse(&feed).unwrap();
        assert_eq!(
            document
                .descendants()
                .filter(|node| node.tag_name().name() == "entry")
                .count(),
            1
        );
        assert!(!feed.contains("Something happened"));

        let robots = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let robots =
            String::from_utf8(to_bytes(robots.into_body(), 4096).await.unwrap().to_vec()).unwrap();
        assert!(robots.contains("Disallow: /dashboard"));

        let reports = app
            .oneshot(
                Request::builder()
                    .uri("/issues.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let reports =
            String::from_utf8(to_bytes(reports.into_body(), 4096).await.unwrap().to_vec()).unwrap();
        assert!(reports.contains("\"status\": \"ok\""));
    }
}
