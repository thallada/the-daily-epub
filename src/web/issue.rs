use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};

use anyhow::Context;
use askama::Template;
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Response};
use axum_login::tower_sessions::Session;
use jiff::civil::Date;
use sqlx::Row;

use crate::db::Db;
use crate::epub::chapters;
use crate::pipeline::display_date;
use crate::server::AppState;
use crate::types::{
    ArticleId, BehindThePaper, Colophon, Edition, Editorial, Issue, IssueMeta, Lineup, Pick,
};
use crate::web::rate::{self, RatingWidget};
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError, take_flash};

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
        (
            "EPUB",
            row.epub_path.as_deref(),
            Some(config.publish.epub_dir.join(crate::publish::issue_filename(
                date,
                Edition::Standard,
                "epub",
            ))),
            "epub",
        ),
        (
            "X4 EPUB",
            row.x4_path.as_deref(),
            Some(config.publish.epub_dir.join(crate::publish::issue_filename(
                date,
                Edition::X4,
                "epub",
            ))),
            "epub",
        ),
        ("XTC", row.xtc_path.as_deref(), None, "xtc"),
    ]
    .into_iter()
    .filter_map(|(label, raw, fallback, kind)| download(label, raw, fallback, kind))
    .collect();
    Ok(Some(IssueView {
        issue,
        downloads,
        from_json,
    }))
}

fn download(
    label: &str,
    raw: Option<&str>,
    fallback: Option<PathBuf>,
    kind: &str,
) -> Option<Download> {
    let path = raw
        .map(FsPath::new)
        .filter(|path| path.is_file())
        .map(FsPath::to_path_buf)
        .or_else(|| fallback.filter(|path| path.is_file()))?;
    let metadata = path.metadata().ok()?;
    let name = path.file_name()?.to_str()?;
    Some(Download {
        label: label.to_string(),
        href: format!("/files/{kind}/{}", crate::web::encode_component(name)),
        size_bytes: metadata.len(),
    })
}

#[derive(Debug)]
struct FullEntry {
    title: String,
    href: String,
    source: String,
    reading_minutes: i64,
    summary: String,
    why: Option<String>,
    rating: Option<RatingWidget>,
}

#[derive(Debug)]
struct FullSection {
    name: String,
    entries: Vec<FullEntry>,
}

#[derive(Debug)]
struct CostLine {
    provider: String,
    cost: String,
}

#[derive(Debug)]
struct ColophonView {
    generated_at: String,
    bulk_model: String,
    editor_model: String,
    summaries_model: String,
    provider_costs: Vec<CostLine>,
    entries_fetched: i64,
    feeds_seen: i64,
    candidates: i64,
    article_count: i64,
    section_count: i64,
    total_words: String,
    reading_minutes: i64,
    cost_usd: String,
    generator_version: String,
}

#[derive(Template)]
#[template(path = "issue_full.html")]
struct IssueFullTemplate {
    page: Page,
    display_date: String,
    issue_number: i64,
    stats_line: String,
    front_page_html: String,
    downloads: Vec<Download>,
    sections: Vec<FullSection>,
    has_world: bool,
    has_behind: bool,
    date: Date,
    colophon: ColophonView,
}

#[derive(Debug)]
struct ArticleLink {
    title: String,
    href: String,
}

#[derive(Template)]
#[template(path = "article.html")]
struct ArticleTemplate {
    page: Page,
    title: String,
    source_url: String,
    byline: Option<String>,
    meta_line: String,
    why: Option<String>,
    social_line: Option<String>,
    summary: Option<String>,
    excerpt_only: bool,
    body_html: String,
    discussion_html: Option<String>,
    read_online_url: String,
    rating: Option<RatingWidget>,
    previous: Option<ArticleLink>,
    next: Option<ArticleLink>,
    issue_href: String,
}

#[derive(Template)]
#[template(path = "world.html")]
struct WorldTemplate {
    page: Page,
    display_date: String,
    body_html: String,
    issue_href: String,
}

#[derive(Debug)]
struct NearMissView {
    article_id: ArticleId,
    line: String,
}

#[derive(Template)]
#[template(path = "behind.html")]
struct BehindTemplate {
    page: Page,
    summary_line: String,
    admitted_line: String,
    learned_line: String,
    near_misses: Vec<NearMissView>,
    models_line: String,
    issue_href: String,
}

pub async fn render_full(
    state: &AppState,
    view: IssueView,
    viewer: Viewer,
    session: &Session,
) -> Result<Response, WebError> {
    let date = view.issue.meta.date;
    let is_admin = viewer.role == crate::web::users::Role::Admin;
    let current = if is_admin {
        rate::current_for_issue(state, date).await?
    } else {
        HashMap::new()
    };
    let issue_href = format!("/issues/{date}");
    let sections = chapters::section_names(&view.issue)
        .into_iter()
        .map(|name| FullSection {
            entries: view
                .issue
                .lineup
                .section_picks(&name)
                .into_iter()
                .map(|pick| FullEntry {
                    title: pick.article.title.clone(),
                    href: article_href(date, pick.article.id),
                    source: pick.article.feed_title.clone(),
                    reading_minutes: pick.article.reading_minutes(),
                    summary: summary_for(&view.issue, pick)
                        .unwrap_or_default()
                        .to_string(),
                    why: pick.why.clone(),
                    rating: is_admin.then(|| {
                        RatingWidget::for_issue(
                            pick.article.id,
                            date,
                            issue_href.clone(),
                            current.get(&pick.article.id).map(String::as_str),
                        )
                    }),
                })
                .collect(),
            name,
        })
        .collect();
    let colophon = colophon_view(&view.issue);
    let mut page = Page::new(format!("Issue {date}"), Some(viewer), "latest");
    page.flash = take_flash(session).await?;
    Ok(Html(IssueFullTemplate {
        page,
        display_date: view.issue.meta.display_date.clone(),
        issue_number: view.issue.meta.issue_number,
        stats_line: view.issue.meta.stats_line(),
        front_page_html: view.issue.editorial.front_page_html.clone(),
        downloads: view.downloads,
        sections,
        has_world: view.issue.world_briefing.is_some(),
        has_behind: view.from_json,
        date,
        colophon,
    })
    .into_response())
}

pub async fn article(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path((date, article_id)): Path<(Date, ArticleId)>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: article_href(date, article_id),
        })?;
    let Some(view) = load(&state.db, &state.config(), date).await? else {
        return Err(WebError::NotFound);
    };
    let Some(index) = view
        .issue
        .lineup
        .picks
        .iter()
        .position(|pick| pick.article.id == article_id)
    else {
        return Err(WebError::NotFound);
    };
    let pick = &view.issue.lineup.picks[index];
    let current = if viewer.role == crate::web::users::Role::Admin {
        rate::current_for_issue(&state, date).await?
    } else {
        HashMap::new()
    };
    let previous = index.checked_sub(1).map(|previous| {
        let article = &view.issue.lineup.picks[previous].article;
        ArticleLink {
            title: article.title.clone(),
            href: article_href(date, article.id),
        }
    });
    let next = view
        .issue
        .lineup
        .picks
        .get(index + 1)
        .map(|next| ArticleLink {
            title: next.article.title.clone(),
            href: article_href(date, next.article.id),
        });
    let article = &pick.article;
    let mut page = Page::new(article.title.clone(), Some(viewer.clone()), "latest");
    page.flash = take_flash(&session).await?;
    Ok(Html(ArticleTemplate {
        page,
        title: article.title.clone(),
        source_url: article.canonical_url.clone(),
        byline: article.author.as_ref().map(|author| format!("By {author}")),
        meta_line: format!(
            "{} · {} words · ~{} min read",
            article.feed_title,
            thousands(article.word_count),
            article.reading_minutes()
        ),
        why: pick.why.clone(),
        social_line: chapters::social_line(&article.social),
        summary: summary_for(&view.issue, pick).map(str::to_string),
        excerpt_only: article.excerpt_only,
        body_html: prepare_body(&article.content_html),
        discussion_html: pick
            .discussion
            .as_ref()
            .map(|discussion| crate::comments::render_xhtml(discussion, &article.title)),
        read_online_url: article.url.clone(),
        rating: (viewer.role == crate::web::users::Role::Admin).then(|| {
            RatingWidget::for_issue(
                article.id,
                date,
                article_href(date, article.id),
                current.get(&article.id).map(String::as_str),
            )
        }),
        previous,
        next,
        issue_href: format!("/issues/{date}"),
    })
    .into_response())
}

pub async fn world(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(date): Path<Date>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: format!("/issues/{date}/world"),
        })?;
    let Some(view) = load(&state.db, &state.config(), date).await? else {
        return Err(WebError::NotFound);
    };
    let Some(briefing) = view.issue.world_briefing else {
        return Err(WebError::NotFound);
    };
    let mut page = Page::new("World Briefing", Some(viewer), "latest");
    page.flash = take_flash(&session).await?;
    Ok(Html(WorldTemplate {
        page,
        display_date: display_date(briefing.date),
        body_html: crate::world::render_xhtml(&briefing),
        issue_href: format!("/issues/{date}"),
    })
    .into_response())
}

pub async fn behind(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Path(date): Path<Date>,
) -> Result<Response, WebError> {
    let viewer = auth
        .user()
        .await
        .map(Viewer::from)
        .ok_or_else(|| WebError::Unauthenticated {
            next: format!("/issues/{date}/behind"),
        })?;
    let Some(view) = load(&state.db, &state.config(), date).await? else {
        return Err(WebError::NotFound);
    };
    if !view.from_json {
        return Err(WebError::NotFound);
    }
    let behind = &view.issue.behind;
    let mut page = Page::new("Behind the paper", Some(viewer), "latest");
    page.flash = take_flash(&session).await?;
    Ok(Html(BehindTemplate {
        page,
        summary_line: chapters::behind_summary_line(behind),
        admitted_line: chapters::behind_admitted_line(behind),
        learned_line: chapters::behind_learned_line(behind),
        near_misses: behind
            .near_misses
            .iter()
            .map(|near_miss| NearMissView {
                article_id: near_miss.article_id,
                line: chapters::behind_near_miss_line(near_miss),
            })
            .collect(),
        models_line: chapters::behind_models_line(behind),
        issue_href: format!("/issues/{date}"),
    })
    .into_response())
}

fn article_href(date: Date, article_id: ArticleId) -> String {
    format!("/issues/{date}/articles/{article_id}")
}

fn summary_for<'a>(issue: &'a Issue, pick: &'a Pick) -> Option<&'a str> {
    pick.summary
        .as_deref()
        .or_else(|| {
            issue
                .editorial
                .summaries
                .get(&pick.article.id)
                .map(String::as_str)
        })
        .filter(|summary| !summary.trim().is_empty())
}

fn colophon_view(issue: &Issue) -> ColophonView {
    let colophon = &issue.colophon;
    ColophonView {
        generated_at: issue.meta.generated_at.to_string(),
        bulk_model: colophon.models.bulk.clone(),
        editor_model: colophon.models.editor.clone(),
        summaries_model: colophon.models.summaries.clone(),
        provider_costs: colophon
            .provider_costs
            .iter()
            .map(|(provider, cost)| CostLine {
                provider: provider.clone(),
                cost: format!("${cost:.4}"),
            })
            .collect(),
        entries_fetched: colophon.entries_fetched,
        feeds_seen: colophon.feeds_seen,
        candidates: colophon.candidates,
        article_count: issue.meta.article_count,
        section_count: issue.meta.section_count,
        total_words: thousands(issue.meta.total_words),
        reading_minutes: issue.meta.reading_minutes,
        cost_usd: format!("${:.4}", colophon.cost_usd),
        generator_version: if colophon.generator_version.is_empty() {
            format!("daily-epub {}", env!("CARGO_PKG_VERSION"))
        } else {
            colophon.generator_version.clone()
        },
    }
}

fn thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    if value < 0 {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// Sanitize article HTML exactly as the EPUB does, then add browser-only image
/// loading/referrer attributes without converting the fragment to XHTML.
fn prepare_body(html: &str) -> String {
    let clean = ammonia::clean(html);
    let mut output = String::with_capacity(clean.len() + 64);
    let mut cursor = 0usize;
    while let Some(relative) = clean[cursor..].find('<') {
        let start = cursor + relative;
        output.push_str(&clean[cursor..start]);
        let Some(end) = crate::html::tag_end(&clean, start) else {
            output.push_str(&clean[start..]);
            return output;
        };
        let raw = &clean[start..end];
        let inner = raw.trim_start_matches('<').trim_end_matches('>');
        if crate::html::tag_name(inner) == "img" {
            let attributes = crate::html::parse_attrs(inner);
            let trimmed = raw.trim_end_matches('>');
            output.push_str(trimmed.trim_end_matches('/'));
            if !attributes.iter().any(|(name, _)| name == "loading") {
                output.push_str(" loading=\"lazy\"");
            }
            if !attributes.iter().any(|(name, _)| name == "referrerpolicy") {
                output.push_str(" referrerpolicy=\"no-referrer\"");
            }
            output.push('>');
        } else {
            output.push_str(raw);
        }
        cursor = end;
    }
    output.push_str(&clean[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use serde_json::json;
    use tower::ServiceExt;

    use crate::types::{Entry, Issue};

    use super::*;

    async fn login_cookie(app: &axum::Router, username: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-forwarded-for", "192.0.2.88")
                    .body(Body::from(format!(
                        "username={username}&password={password}&next=%2F"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    async fn response_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

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

    #[tokio::test]
    async fn signed_in_full_issue_article_world_and_behind_render_private_content() {
        let (_dir, db, source) = seeded_issue(true).await;
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = crate::server::router(crate::server::AppState::new(
            db,
            crate::config::Config::default(),
            None,
        ));
        let anonymous = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/issues/{}/articles/{}",
                        source.meta.date, source.lineup.picks[0].article.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::FOUND);
        let cookie = login_cookie(&app, "reader", "correct horse battery").await;

        let issue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}", source.meta.date))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issue.status(), StatusCode::OK);
        assert_eq!(
            issue.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        let issue = response_text(issue).await;
        assert!(issue.contains("The Brief"));
        assert!(issue.contains("Two stories today"));
        assert!(issue.contains("What it argues"));
        assert!(issue.contains("A short abstract for the second piece"));
        assert!(issue.contains("Why it"));
        assert!(issue.contains("World Briefing"));
        assert!(issue.contains("Behind the paper"));
        assert!(!issue.contains("Was this a good pick?"));

        let article_id = source.lineup.picks[0].article.id;
        let article = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/issues/{}/articles/{article_id}",
                        source.meta.date
                    ))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(article.status(), StatusCode::OK);
        let article = response_text(article).await;
        assert!(article.contains("Body of <em>The Lead Story</em>"));
        assert!(article.contains("The write path is the interesting part"));
        assert!(article.contains("loading=\"lazy\""));
        assert!(article.contains("referrerpolicy=\"no-referrer\""));
        assert!(article.contains("A Niche Delight"));
        assert!(article.contains("rel=\"next\""));

        let world = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/world", source.meta.date))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(world.status(), StatusCode::OK);
        assert!(
            response_text(world)
                .await
                .contains("Something happened somewhere")
        );

        let behind = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/behind", source.meta.date))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(behind.status(), StatusCode::OK);
        let behind = response_text(behind).await;
        assert!(behind.contains("Considered 412 articles"));
        assert!(!behind.contains("/dashboard/articles/3"));

        let missing = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/articles/999999", source.meta.date))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(response_text(missing).await.contains("Not found"));
    }

    #[tokio::test]
    async fn fallback_full_issue_omits_ephemeral_chapter_links() {
        let (_dir, db, source) = seeded_issue(false).await;
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = crate::server::router(crate::server::AppState::new(
            db,
            crate::config::Config::default(),
            None,
        ));
        let cookie = login_cookie(&app, "reader", "correct horse battery").await;
        let issue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}", source.meta.date))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issue.status(), StatusCode::OK);
        let issue = response_text(issue).await;
        assert!(issue.contains("Two stories today"));
        assert!(!issue.contains(&format!("/issues/{}/world", source.meta.date)));
        assert!(!issue.contains(&format!("/issues/{}/behind", source.meta.date)));

        let world = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/world", source.meta.date))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(world.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn downloads_are_listed_only_while_the_files_exist() {
        let (dir, db, source) = seeded_issue(true).await;
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let epub_dir = dir.path().join("epubs");
        std::fs::create_dir(&epub_dir).unwrap();
        let standard = epub_dir.join(crate::publish::issue_filename(
            source.meta.date,
            Edition::Standard,
            "epub",
        ));
        std::fs::write(&standard, b"epub").unwrap();
        let mut config = crate::config::Config::default();
        config.publish.epub_dir = epub_dir;
        let app = crate::server::router(crate::server::AppState::new(db, config, None));
        let cookie = login_cookie(&app, "reader", "correct horse battery").await;
        let issue = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}", source.meta.date))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let issue = response_text(issue).await;
        assert!(issue.contains("Download EPUB"));
        assert!(!issue.contains("Download X4 EPUB"));
        assert!(!issue.contains("Download XTC"));
    }

    #[tokio::test]
    async fn rating_post_supports_json_forms_attribution_fallback_and_clear() {
        let (_dir, db, source) = seeded_issue(true).await;
        let admin = crate::web::users::add(&db, "admin", "correct horse battery", true)
            .await
            .unwrap();
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = crate::server::router(crate::server::AppState::new(
            db.clone(),
            crate::config::Config::default(),
            None,
        ));
        let admin_cookie = login_cookie(&app, "admin", "correct horse battery").await;
        let reader_cookie = login_cookie(&app, "reader", "correct horse battery").await;
        let article_id = source.lineup.picks[0].article.id;

        let json_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(
                        json!({"article_id": article_id, "label": "loved"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_response.status(), StatusCode::OK);
        let json_body: serde_json::Value =
            serde_json::from_str(&response_text(json_response).await).unwrap();
        assert_eq!(json_body["article_id"], article_id);
        assert_eq!(json_body["label"], "loved");
        assert!(json_body["event_id"].as_i64().is_some());
        let stored = sqlx::query(
            "SELECT source, user_id, issue_date, label, value FROM rating_events ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(stored.get::<String, _>("source"), "dashboard");
        assert_eq!(stored.get::<Option<i64>, _>("user_id"), Some(admin.id));
        assert_eq!(
            stored.get::<Option<String>, _>("issue_date").as_deref(),
            Some(source.meta.date.to_string().as_str())
        );

        let admin_issue = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}", source.meta.date))
                    .header(header::COOKIE, &admin_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let admin_issue = response_text(admin_issue).await;
        assert!(admin_issue.contains("Was this a good pick?"));
        assert!(admin_issue.contains("value=\"loved\" data-label=\"loved\" class=\"active\""));

        let admin_behind = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/behind", source.meta.date))
                    .header(header::COOKIE, &admin_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response_text(admin_behind)
                .await
                .contains("/dashboard/articles/3")
        );

        let clear = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(format!(
                        "article_id={article_id}&label=cleared&next=https%3A%2F%2Fevil.example"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(clear.status(), StatusCode::SEE_OTHER);
        assert_eq!(clear.headers().get(header::LOCATION).unwrap(), "/");
        let cleared =
            sqlx::query("SELECT label, value FROM rating_events ORDER BY id DESC LIMIT 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(cleared.get::<String, _>("label"), "cleared");
        assert_eq!(cleared.get::<f64, _>("value"), 0.0);

        let form = format!(
            "article_id={article_id}&issue_date={}&label=down&next=%2Fissues%2F{}",
            source.meta.date, source.meta.date
        );
        let forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, reader_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(form.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let anonymous = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(form.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::FOUND);

        let valid = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/rate")
                    .header(header::COOKIE, admin_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            valid.headers().get(header::LOCATION).unwrap(),
            format!("/issues/{}", source.meta.date).as_str()
        );
        let down: String =
            sqlx::query_scalar("SELECT label FROM rating_events ORDER BY id DESC LIMIT 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(down, "not_for_me");
    }

    #[test]
    fn web_body_is_sanitized_and_images_get_browser_attributes() {
        let body = prepare_body(
            r#"<script>alert(1)</script><img src="https://img.example/a.png" alt="chart"><p>safe</p>"#,
        );
        assert!(!body.contains("<script"));
        assert!(body.contains("src=\"https://img.example/a.png\""));
        assert!(body.contains("loading=\"lazy\""));
        assert!(body.contains("referrerpolicy=\"no-referrer\""));
    }
}
