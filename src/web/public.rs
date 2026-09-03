use askama::Template;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_login::tower_sessions::Session;
use jiff::civil::Date;

use crate::server::AppState;
use crate::types::{Issue, SocialSource};
use crate::web::issue::{self, Download};
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError};

#[derive(Debug, Clone)]
pub struct PublicIssue {
    pub date: Date,
    pub issue_number: i64,
    pub display_date: String,
    pub article_count: i64,
    pub reading_minutes: i64,
    pub stats_line: String,
    pub sections: Vec<PublicSection>,
    pub generated_at: jiff::Timestamp,
}

#[derive(Debug, Clone)]
pub struct PublicSection {
    pub name: String,
    pub entries: Vec<PublicEntry>,
}

#[derive(Debug, Clone)]
pub struct PublicEntry {
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub source: String,
    pub domain: String,
    pub reading_minutes: i64,
    pub word_count: i64,
    pub summary: Option<String>,
    pub why: Option<String>,
    pub comment_links: Vec<CommentLink>,
    pub is_lead: bool,
}

#[derive(Debug, Clone)]
pub struct CommentLink {
    pub label: String,
    pub url: String,
    pub meta: String,
}

impl From<&Issue> for PublicIssue {
    fn from(issue: &Issue) -> Self {
        let sections =
            issue
                .lineup
                .section_order
                .iter()
                .map(|name| PublicSection {
                    name: name.clone(),
                    entries: issue
                        .lineup
                        .section_picks(name)
                        .into_iter()
                        .map(|pick| {
                            let article = &pick.article;
                            let mut comment_links: Vec<CommentLink> = article
                                .social
                                .iter()
                                .filter_map(|social| {
                                    let url = social.item_url.clone()?;
                                    let label = match social.source {
                                        SocialSource::Hn => "Hacker News",
                                        SocialSource::Lobsters => "Lobsters",
                                        SocialSource::Reddit => "Reddit",
                                        SocialSource::X => "X",
                                    };
                                    Some(CommentLink {
                                        label: label.into(),
                                        url,
                                        meta: format!(
                                            "{} points · {} comments",
                                            social.score, social.num_comments
                                        ),
                                    })
                                })
                                .collect();
                            if let Some(url) = article.comments_url.as_ref().filter(|url| {
                                *url != &article.canonical_url && *url != &article.url
                            }) {
                                comment_links.push(CommentLink {
                                    label: "Comments".into(),
                                    url: url.clone(),
                                    meta: String::new(),
                                });
                            }
                            PublicEntry {
                                title: article.title.clone(),
                                url: article.canonical_url.clone(),
                                author: article.author.clone(),
                                source: article.feed_title.clone(),
                                domain: domain(&article.canonical_url),
                                reading_minutes: article.reading_minutes(),
                                word_count: article.word_count,
                                summary: pick
                                    .summary
                                    .as_deref()
                                    .or_else(|| {
                                        issue
                                            .editorial
                                            .summaries
                                            .get(&article.id)
                                            .map(String::as_str)
                                    })
                                    .map(str::trim)
                                    .filter(|summary| !summary.is_empty())
                                    .map(str::to_string),
                                why: pick.why.clone(),
                                comment_links,
                                is_lead: pick.is_lead,
                            }
                        })
                        .collect(),
                })
                .collect();
        Self {
            date: issue.meta.date,
            issue_number: issue.meta.issue_number,
            display_date: issue.meta.display_date.clone(),
            article_count: issue.meta.article_count,
            reading_minutes: issue.meta.reading_minutes,
            stats_line: issue.meta.stats_line(),
            sections,
            generated_at: issue.meta.generated_at,
        }
    }
}

fn domain(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .map(|host| host.strip_prefix("www.").unwrap_or(&host).to_string())
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "issue_public.html")]
struct IssuePublicTemplate {
    page: Page,
    issue: PublicIssue,
    downloads: Vec<Download>,
    empty: bool,
}

#[derive(Debug, Clone)]
pub struct ArchiveMonth {
    pub label: String,
    pub issues: Vec<ArchiveIssue>,
}

#[derive(Debug, Clone)]
pub struct ArchiveIssue {
    pub date: Date,
    pub display_date: String,
    pub issue_number: i64,
    pub article_count: i64,
}

#[derive(Template)]
#[template(path = "issue_list.html")]
struct IssueListTemplate {
    page: Page,
    months: Vec<ArchiveMonth>,
}

#[derive(Template)]
#[template(path = "feed_entry.html")]
struct FeedEntryTemplate<'a> {
    issue: &'a PublicIssue,
}

pub async fn latest(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let Some(date) = state.db.latest_issue_date().await? else {
        let viewer = auth.user().await.map(Viewer::from);
        let response = Html(IssuePublicTemplate {
            page: Page::new("Latest issue", viewer, "latest"),
            issue: empty_issue(),
            downloads: Vec::new(),
            empty: true,
        })
        .into_response();
        return Ok(public_cache(response, &headers));
    };
    show_issue(State(state), auth, Extension(session), headers, Path(date)).await
}

pub async fn show_issue(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Path(date): Path<Date>,
) -> Result<Response, WebError> {
    let Some(view) = issue::load(&state.db, &state.config(), date).await? else {
        return Err(WebError::NotFound);
    };
    let viewer = auth.user().await.map(Viewer::from);
    if let Some(viewer) = viewer {
        let response = issue::render_full(&state, view, viewer, &session).await?;
        return Ok(public_cache(response, &headers));
    }
    let response = Html(IssuePublicTemplate {
        page: Page::new(format!("Issue {date}"), None, "latest"),
        issue: PublicIssue::from(&view.issue),
        downloads: Vec::new(),
        empty: false,
    })
    .into_response();
    Ok(public_cache(response, &headers))
}

pub async fn archive(
    State(state): State<AppState>,
    auth: AuthSession,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let rows = state.db.issue_dates(None).await?;
    let mut months: Vec<ArchiveMonth> = Vec::new();
    for row in rows {
        let key = format!("{:04}-{:02}", row.date.year(), row.date.month());
        if months.last().map(|month| month.label.as_str()) != Some(key.as_str()) {
            months.push(ArchiveMonth {
                label: key,
                issues: Vec::new(),
            });
        }
        if let Some(month) = months.last_mut() {
            month.issues.push(ArchiveIssue {
                date: row.date,
                display_date: crate::pipeline::display_date(row.date),
                issue_number: row.issue_number,
                article_count: row.article_count,
            });
        }
    }
    let response = Html(IssueListTemplate {
        page: Page::new(
            "Issue archive",
            auth.user().await.map(Viewer::from),
            "archive",
        ),
        months,
    })
    .into_response();
    Ok(public_cache(response, &headers))
}

pub async fn feed(State(state): State<AppState>) -> Result<Response, WebError> {
    let config = state.config();
    let rows = state.db.issue_dates(Some(30)).await?;
    let mut entries = String::new();
    let mut updated = jiff::Timestamp::UNIX_EPOCH;
    for row in rows {
        let Some(view) = issue::load(&state.db, &config, row.date).await? else {
            continue;
        };
        updated = updated.max(view.issue.meta.generated_at);
        let issue = PublicIssue::from(&view.issue);
        let content = FeedEntryTemplate { issue: &issue }
            .render()
            .map_err(|error| WebError::Internal(error.into()))?;
        let href = format!(
            "{}/issues/{}",
            config.server.public_url.trim_end_matches('/'),
            issue.date
        );
        entries.push_str(&format!(
            "<entry><id>tag:{},{}:issue/{}</id><title>The Daily EPUB — {}</title><updated>{}</updated><link rel=\"alternate\" href=\"{}\"/><content type=\"html\">{}</content></entry>",
            feed_host(&config.server.public_url),
            issue.date.year(),
            issue.date,
            issue.date,
            issue.generated_at,
            xml_escape(&href),
            xml_escape(&content),
        ));
    }
    let home = config.server.public_url.trim_end_matches('/');
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><id>{}</id><title>The Daily EPUB</title><updated>{}</updated><link rel=\"self\" href=\"{}/feed.xml\"/>{}</feed>",
        xml_escape(home),
        updated,
        xml_escape(home),
        entries
    );
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/atom+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response())
}

pub async fn robots() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\nAllow: /issues\nDisallow: /dashboard\nDisallow: /login\nDisallow: /files\nDisallow: /r\nDisallow: /opds\n",
    )
        .into_response()
}

fn public_cache(mut response: Response, request_headers: &HeaderMap) -> Response {
    let value = if request_headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookies| cookies.contains("daily_session="))
    {
        "private, no-store"
    } else {
        "public, max-age=300"
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(value),
    );
    response
}

fn feed_host(public_url: &str) -> String {
    url::Url::parse(public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "daily.hallada.net".into())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn empty_issue() -> PublicIssue {
    PublicIssue {
        date: "1970-01-01".parse().expect("valid epoch date"),
        issue_number: 0,
        display_date: String::new(),
        article_count: 0,
        reading_minutes: 0,
        stats_line: String::new(),
        sections: Vec::new(),
        generated_at: jiff::Timestamp::UNIX_EPOCH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_issue_shows_summaries_and_why_but_no_bodies() {
        let source = crate::epub::fixtures::issue();
        let public = PublicIssue::from(&source);
        let html = FeedEntryTemplate { issue: &public }.render().unwrap();
        assert!(html.contains("The Lead Story"));
        assert!(html.contains("Hacker News"));
        assert!(html.contains("What it argues, and why it is worth the time."));
        assert!(html.contains("A short abstract for the second piece."));
        assert!(html.contains("The systems story with enough operational detail to matter"));
        assert!(html.contains("A small-scene delight outside the usual technical orbit"));
        for private in [
            "Two stories today",
            "Body of",
            "write path",
            "Agreed",
            "Something happened",
            "concise view of the day",
        ] {
            assert!(!html.contains(private), "leaked {private:?} in {html}");
        }
    }
}
