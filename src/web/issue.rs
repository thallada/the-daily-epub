use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
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
    ArticleId, BehindThePaper, Colophon, Edition, Editorial, Issue, IssueMeta, Lineup, Models, Pick,
};
use crate::web::rate::{self, RatingWidget};
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError, take_flash};

#[derive(Debug, Clone)]
pub struct Download {
    pub label: String,
    pub href: String,
    pub size_bytes: u64,
    pub size: String,
}

#[derive(Debug, Clone)]
pub struct IssueView {
    pub issue: Issue,
    pub downloads: Vec<Download>,
    pub from_json: bool,
    pub world_html: Option<String>,
    pub is_latest: bool,
    has_behind: bool,
    legacy_counts: Option<LegacyColophonCounts>,
}

#[derive(Debug, Clone, Default)]
struct LegacyColophonCounts {
    entries_fetched: Option<i64>,
    feeds_seen: Option<i64>,
    candidates: Option<i64>,
    cost_usd: Option<f64>,
}

#[derive(Debug, Default)]
struct LegacyFacts {
    has_source: bool,
    run_id: Option<i64>,
    entries_fetched: Option<i64>,
    feeds_seen: Option<i64>,
    considered: Option<i64>,
    eligible: Option<i64>,
    triaged: Option<i64>,
    assessed: Option<i64>,
    shortlisted: Option<i64>,
    candidates: Option<i64>,
    selected: Option<i64>,
    admitted_by: BTreeMap<String, i64>,
    rated_with_embeddings: Option<i64>,
    knn_gate: Option<f64>,
    feed_gate: Option<f64>,
    embedded: Option<i64>,
    provider_costs: BTreeMap<String, f64>,
    cost_usd: Option<f64>,
    config_json: Option<serde_json::Value>,
    started_at: Option<jiff::Timestamp>,
    finished_at: Option<jiff::Timestamp>,
}

pub async fn load(
    db: &Db,
    config: &crate::config::Config,
    date: Date,
) -> anyhow::Result<Option<IssueView>> {
    let Some(row) = db.issue_by_date(date).await? else {
        return Ok(None);
    };
    let is_latest = db.latest_issue_date().await? == Some(date);
    let legacy = if row.issue_json.is_none() {
        Some(load_legacy_facts(db, date, row.report_json.as_deref()).await?)
    } else {
        None
    };
    let (mut issue, from_json) = if let Some(raw) = row.issue_json.as_deref() {
        let mut issue: Issue = serde_json::from_str(raw).context("decoding issues.issue_json")?;
        // The snapshot's articles are refreshed from the live rows (social
        // scores move after publish) in one batched lookup.
        let ids: Vec<ArticleId> = issue
            .lineup
            .picks
            .iter()
            .map(|pick| pick.article.id)
            .collect();
        let mut articles = db.get_articles(&ids).await?;
        for pick in &mut issue.lineup.picks {
            if let Some(article) = articles.remove(&pick.article.id) {
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
        let ids: Vec<ArticleId> = rows.iter().map(|row| row.get("article_id")).collect();
        let mut articles = db.get_articles(&ids).await?;
        let mut picks = Vec::with_capacity(rows.len());
        let mut seen_sections = Vec::new();
        let mut summaries = BTreeMap::new();
        for pick_row in rows {
            let article_id: i64 = pick_row.get("article_id");
            let Some(article) = articles.remove(&article_id) else {
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
        let Some(facts) = legacy.as_ref() else {
            return Err(anyhow::anyhow!("legacy issue facts were not loaded"));
        };
        let (models, embedding_model, models_from_current_config) =
            legacy_models(facts.config_json.as_ref(), facts.embedded, config);
        let near_misses = if let Some(run_id) = facts.run_id {
            match crate::curate::telemetry::paper_near_misses(db, run_id, 10).await {
                Ok(near_misses) => near_misses,
                Err(error) => {
                    tracing::debug!(%error, %date, run_id, "could not recover legacy near misses");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let generation_secs = facts
            .started_at
            .zip(facts.finished_at)
            .map(|(started, finished)| (finished.as_second() - started.as_second()).max(0))
            .unwrap_or(0);
        let generator_version = if models_from_current_config {
            "not recorded (issue predates snapshots; model names from the current configuration)"
                .to_string()
        } else {
            "not recorded (issue predates snapshots)".to_string()
        };
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
                    front_page_html: row.front_page_html.clone().unwrap_or_default(),
                    summaries,
                },
                world_briefing: None,
                colophon: Colophon {
                    provider_costs: facts.provider_costs.clone(),
                    models: models.clone(),
                    entries_fetched: facts.entries_fetched.unwrap_or(0),
                    feeds_seen: facts.feeds_seen.unwrap_or(0),
                    candidates: facts.candidates.unwrap_or(0),
                    cost_usd: facts.cost_usd.unwrap_or(0.0),
                    generator_version,
                },
                behind: BehindThePaper {
                    considered: facts.considered.unwrap_or(0),
                    feeds_seen: facts.feeds_seen.unwrap_or(0),
                    eligible: facts.eligible.unwrap_or(0),
                    triaged: facts.triaged.unwrap_or(0),
                    read_closely: facts.assessed.unwrap_or(0),
                    shortlisted: facts.shortlisted.unwrap_or(0),
                    selected: facts.selected.unwrap_or(article_count),
                    admitted_by: facts.admitted_by.clone(),
                    rated_with_embeddings: facts.rated_with_embeddings.unwrap_or(0),
                    knn_gate: facts.knn_gate.unwrap_or(0.0),
                    feed_gate: facts.feed_gate.unwrap_or(0.0),
                    near_misses,
                    models,
                    embedding_model,
                    cost_usd: facts.cost_usd.unwrap_or(0.0),
                    generation_secs,
                },
            },
            false,
        )
    };
    issue.meta.article_count = issue.lineup.picks.len() as i64;
    let world_html = if from_json || issue.world_briefing.is_some() {
        None
    } else {
        recover_world_html(&row, config)
    };
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
        world_html,
        is_latest,
        has_behind: from_json || legacy.as_ref().is_some_and(|facts| facts.has_source),
        legacy_counts: legacy.map(|facts| LegacyColophonCounts {
            entries_fetched: facts.entries_fetched,
            feeds_seen: facts.feeds_seen,
            candidates: facts.candidates,
            cost_usd: facts.cost_usd,
        }),
    }))
}

async fn load_legacy_facts(
    db: &Db,
    date: Date,
    issue_report_json: Option<&str>,
) -> anyhow::Result<LegacyFacts> {
    let issue_report =
        issue_report_json.and_then(|raw| parse_legacy_json(raw, "issues.report_json"));
    let report_started_at = issue_report
        .as_ref()
        .and_then(|report| report.get("started_at"))
        .and_then(serde_json::Value::as_str);
    let run = if let Some(started_at) = report_started_at {
        sqlx::query(
            "SELECT id, started_at, finished_at, entries_fetched, candidates, selected,
                    cost_usd, provider_costs_json, config_json, report_json
             FROM runs
             WHERE date = ? AND started_at = ? AND finished_at IS NOT NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(date.to_string())
        .bind(started_at)
        .fetch_optional(db.pool())
        .await?
    } else {
        sqlx::query(
            "SELECT id, started_at, finished_at, entries_fetched, candidates, selected,
                    cost_usd, provider_costs_json, config_json, report_json
             FROM runs
             WHERE date = ? AND finished_at IS NOT NULL
             ORDER BY finished_at DESC, id DESC LIMIT 1",
        )
        .bind(date.to_string())
        .fetch_optional(db.pool())
        .await?
    };
    let run_report = run
        .as_ref()
        .and_then(|row| row.get::<Option<String>, _>("report_json"))
        .as_deref()
        .and_then(|raw| parse_legacy_json(raw, "runs.report_json"));
    let report = issue_report.as_ref().or(run_report.as_ref());

    let run_config = run
        .as_ref()
        .and_then(|row| row.get::<Option<String>, _>("config_json"))
        .as_deref()
        .and_then(|raw| parse_legacy_json(raw, "runs.config_json"));
    let config_json = report
        .and_then(|value| value.get("config_json"))
        .filter(|value| !value.is_null())
        .cloned()
        .or(run_config);

    let run_provider_costs = run
        .as_ref()
        .and_then(|row| row.get::<Option<String>, _>("provider_costs_json"))
        .as_deref()
        .and_then(|raw| parse_legacy_json(raw, "runs.provider_costs_json"))
        .as_ref()
        .map(provider_costs)
        .unwrap_or_default();
    let report_provider_costs = report
        .and_then(|value| value.get("provider_costs"))
        .map(provider_costs)
        .unwrap_or_default();

    let report_timestamp = |name: &str| {
        report
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| raw.parse().ok())
    };
    let run_timestamp = |name: &str| {
        run.as_ref()
            .and_then(|row| row.get::<Option<String>, _>(name))
            .and_then(|raw| raw.parse().ok())
    };
    let run_i64 = |name: &str| run.as_ref().map(|row| row.get::<i64, _>(name));
    let run_f64 = |name: &str| run.as_ref().map(|row| row.get::<f64, _>(name));

    Ok(LegacyFacts {
        has_source: issue_report_json.is_some() || run.is_some(),
        run_id: run.as_ref().map(|row| row.get("id")),
        entries_fetched: report_count(report, "entries_fetched")
            .or_else(|| run_i64("entries_fetched")),
        feeds_seen: report_count(report, "feeds_seen"),
        considered: report_count(report, "articles"),
        eligible: report_count(report, "eligible"),
        triaged: report_count(report, "triaged"),
        assessed: report_count(report, "assessed"),
        shortlisted: report_count(report, "shortlisted"),
        candidates: report_count(report, "candidates").or_else(|| run_i64("candidates")),
        selected: report_count(report, "selected").or_else(|| run_i64("selected")),
        admitted_by: report
            .and_then(|value| value.pointer("/counts/admitted_by"))
            .and_then(serde_json::Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(name, count)| count.as_i64().map(|count| (name.clone(), count)))
                    .collect()
            })
            .unwrap_or_default(),
        rated_with_embeddings: report_count(report, "rated_with_embeddings"),
        knn_gate: report_count_f64(report, "knn_gate"),
        feed_gate: report_count_f64(report, "feed_gate"),
        embedded: report_count(report, "embedded"),
        provider_costs: if report_provider_costs.is_empty() {
            run_provider_costs
        } else {
            report_provider_costs
        },
        cost_usd: report
            .and_then(|value| value.get("cost_usd"))
            .and_then(serde_json::Value::as_f64)
            .or_else(|| run_f64("cost_usd")),
        config_json,
        started_at: report_timestamp("started_at").or_else(|| run_timestamp("started_at")),
        finished_at: report_timestamp("finished_at").or_else(|| run_timestamp("finished_at")),
    })
}

fn parse_legacy_json(raw: &str, column: &str) -> Option<serde_json::Value> {
    match serde_json::from_str(raw) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(%error, column, "could not decode legacy issue metadata");
            None
        }
    }
}

fn report_count(report: Option<&serde_json::Value>, name: &str) -> Option<i64> {
    report
        .and_then(|value| value.get("counts"))
        .and_then(|counts| counts.get(name))
        .and_then(serde_json::Value::as_i64)
}

fn report_count_f64(report: Option<&serde_json::Value>, name: &str) -> Option<f64> {
    report
        .and_then(|value| value.get("counts"))
        .and_then(|counts| counts.get(name))
        .and_then(serde_json::Value::as_f64)
}

fn provider_costs(value: &serde_json::Value) -> BTreeMap<String, f64> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(provider, usage)| {
            usage
                .as_f64()
                .or_else(|| usage.get("cost_usd").and_then(serde_json::Value::as_f64))
                .map(|cost| (provider.clone(), cost))
        })
        .collect()
}

fn legacy_models(
    stored: Option<&serde_json::Value>,
    embedded: Option<i64>,
    config: &crate::config::Config,
) -> (Models, String, bool) {
    let stored_model = |role: &str| {
        stored
            .and_then(|value| value.pointer(&format!("/models/{role}")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                stored
                    .and_then(|value| value.pointer(&format!("/llm/{role}/model")))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                let provider = stored
                    .and_then(|value| value.pointer(&format!("/llm/{role}")))
                    .and_then(serde_json::Value::as_str)?;
                stored
                    .and_then(|value| value.pointer(&format!("/providers/{provider}/model")))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
    };

    let current_bulk = || {
        config
            .bulk_provider()
            .map(|(_, provider)| provider.model.clone())
            .unwrap_or_else(|| "none".into())
    };
    let mut used_current = false;
    let bulk = stored_model("bulk").unwrap_or_else(|| {
        used_current = true;
        current_bulk()
    });
    let stored_editor = stored_model("editor");
    let editor = match stored_editor.as_deref() {
        Some("disabled") | Some("none") => format!("{bulk} (bulk fallback)"),
        Some(editor) => editor.to_string(),
        None => {
            used_current = true;
            config
                .editor_provider()
                .map(|(_, provider)| provider.model.clone())
                .unwrap_or_else(|| format!("{bulk} (bulk fallback)"))
        }
    };
    let summary_role = stored
        .and_then(|value| value.pointer("/editorial/summary_model"))
        .and_then(serde_json::Value::as_str);
    let summaries = match summary_role {
        Some("bulk") => bulk.clone(),
        Some("editor") => match stored_editor.as_deref() {
            Some("disabled") | Some("none") => bulk.clone(),
            _ => editor.clone(),
        },
        _ => {
            used_current = true;
            match config.editorial.summary_model {
                crate::config::SummaryModel::Editor if config.editor_provider().is_some() => config
                    .editor_provider()
                    .map(|(_, provider)| provider.model.clone())
                    .unwrap_or_else(|| bulk.clone()),
                _ => bulk.clone(),
            }
        }
    };
    let configured_embedding = stored
        .and_then(|value| value.pointer("/models/embedding"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            stored
                .and_then(|value| value.pointer("/voyage/model"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string)
        .unwrap_or_else(|| {
            used_current = true;
            if config.voyage.enabled {
                config.voyage.model.clone()
            } else {
                "disabled".into()
            }
        });
    let embedding = if embedded == Some(0) {
        "none".to_string()
    } else {
        configured_embedding
    };
    (
        Models {
            bulk,
            editor,
            summaries,
        },
        embedding,
        used_current,
    )
}

fn recover_world_html(row: &crate::db::IssueRow, config: &crate::config::Config) -> Option<String> {
    let fallback = config.publish.epub_dir.join(crate::publish::issue_filename(
        row.date,
        Edition::Standard,
        "epub",
    ));
    let path = row
        .epub_path
        .as_deref()
        .map(FsPath::new)
        .filter(|path| path.is_file())
        .map(FsPath::to_path_buf)
        .or_else(|| fallback.is_file().then_some(fallback))?;
    match read_world_html(&path) {
        Ok(html) => Some(html),
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "could not recover legacy World Briefing");
            None
        }
    }
}

fn read_world_html(path: &FsPath) -> anyhow::Result<String> {
    let file = std::fs::File::open(path).context("opening legacy EPUB")?;
    let mut archive = zip::ZipArchive::new(file).context("opening legacy EPUB archive")?;
    let mut chapter = archive
        .by_name("OEBPS/world.xhtml")
        .context("reading OEBPS/world.xhtml")?;
    let mut xhtml = String::new();
    chapter
        .read_to_string(&mut xhtml)
        .context("decoding OEBPS/world.xhtml")?;
    world_body_without_heading(&xhtml).context("finding the World Briefing body")
}

fn world_body_without_heading(xhtml: &str) -> Option<String> {
    let body_start = xhtml.find("<body")?;
    let content_start = crate::html::tag_end(xhtml, body_start)?;
    let content_end = xhtml[content_start..].rfind("</body>")? + content_start;
    let mut body = xhtml[content_start..content_end].to_string();
    if let Some(heading_start) = body.find("<h1")
        && let Some(heading_open_end) = crate::html::tag_end(&body, heading_start)
        && let Some(relative_end) = body[heading_open_end..].find("</h1>")
    {
        let heading_end = heading_open_end + relative_end + "</h1>".len();
        body.replace_range(heading_start..heading_end, "");
    }
    Some(body.trim().to_string()).filter(|body| !body.is_empty())
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
        size: format_file_size(metadata.len()),
    })
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes_float = bytes as f64;
    if bytes_float >= GB {
        format!("{:.1} GB", bytes_float / GB)
    } else if bytes_float >= MB {
        format!("{:.1} MB", bytes_float / MB)
    } else if bytes_float >= KB {
        format!("{:.1} KB", bytes_float / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Where the reader is inside the issue, for [`issue_toc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TocPosition {
    FrontPage,
    Article(ArticleId),
    World,
    Behind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TocKind {
    Brief,
    Section,
    Chapter,
    World,
    Behind,
    Colophon,
}

#[derive(Debug)]
struct TocItem {
    kind: TocKind,
    label: String,
    href: String,
    number: Option<usize>,
    minutes: Option<i64>,
    /// Progress position used by the shared TOC UI (0 for The Brief).
    progress: usize,
    current: bool,
    /// First entry of the back-matter group; the template draws a hairline above it.
    divider: bool,
}

impl TocItem {
    fn is_section(&self) -> bool {
        matches!(self.kind, TocKind::Section)
    }

    fn is_colophon(&self) -> bool {
        matches!(self.kind, TocKind::Colophon)
    }
}

/// The table of contents shared by the four signed-in issue pages.
#[derive(Debug)]
struct Toc {
    display_date: String,
    short_date: String,
    issue_number: i64,
    items: Vec<TocItem>,
    /// 1-based index of the current chapter among the navigable ones; 0 on the issue page.
    position: usize,
    /// Navigable chapters: articles plus World Briefing and Behind the paper.
    total: usize,
    issue_href: String,
}

impl Toc {
    /// Title shown next to the hamburger on narrow screens.
    fn current_label(&self) -> &str {
        self.items
            .iter()
            .find(|item| item.current)
            .map_or("Front page", |item| item.label.as_str())
    }
}

/// Build the chapter list for `view`, marking `current`.
///
/// Chapters are the picks in issue order, numbered `1..n` across sections, with
/// the section names interleaved as non-links; the World Briefing and Behind the
/// paper chapters follow when the issue has them, then the colophon anchor.
fn issue_toc(view: &IssueView, current: TocPosition) -> Toc {
    let issue = &view.issue;
    let date = issue.meta.date;
    let issue_href = issue_href(date);
    let plain = |kind: TocKind,
                 label: &str,
                 href: String,
                 progress: usize,
                 current: bool,
                 divider: bool| TocItem {
        kind,
        label: label.to_string(),
        href,
        number: None,
        minutes: None,
        progress,
        current,
        divider,
    };

    let mut items = vec![plain(
        TocKind::Brief,
        "The Brief",
        issue_href.clone(),
        0,
        current == TocPosition::FrontPage,
        false,
    )];
    let mut position = 0usize;
    let mut total = 0usize;
    for name in chapters::section_names(issue) {
        items.push(plain(
            TocKind::Section,
            &name,
            String::new(),
            0,
            false,
            false,
        ));
        for pick in issue.lineup.section_picks(&name) {
            total += 1;
            let is_current = current == TocPosition::Article(pick.article.id);
            if is_current {
                position = total;
            }
            items.push(TocItem {
                kind: TocKind::Chapter,
                label: pick.article.title.clone(),
                href: article_href(date, pick.article.id),
                number: Some(total),
                minutes: Some(pick.article.reading_minutes()),
                progress: total,
                current: is_current,
                divider: false,
            });
        }
    }

    let mut divider = true;
    if issue.world_briefing.is_some() || view.world_html.is_some() {
        total += 1;
        let is_current = current == TocPosition::World;
        if is_current {
            position = total;
        }
        items.push(plain(
            TocKind::World,
            "World Briefing",
            format!("/issues/{date}/world"),
            total,
            is_current,
            divider,
        ));
        divider = false;
    }
    if view.has_behind {
        total += 1;
        let is_current = current == TocPosition::Behind;
        if is_current {
            position = total;
        }
        items.push(plain(
            TocKind::Behind,
            "Behind the paper",
            format!("/issues/{date}/behind"),
            total,
            is_current,
            divider,
        ));
        divider = false;
    }
    items.push(plain(
        TocKind::Colophon,
        "Colophon",
        format!("{issue_href}#colophon"),
        total,
        false,
        divider,
    ));

    Toc {
        display_date: issue.meta.display_date.clone(),
        short_date: crate::pipeline::short_display_date(date),
        issue_number: issue.meta.issue_number,
        items,
        position,
        total,
        issue_href,
    }
}

#[derive(Debug)]
struct FullEntry {
    title: String,
    href: String,
    source: String,
    reading_minutes: i64,
    is_lead: bool,
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
    entries_fetched: Option<i64>,
    feeds_seen: Option<i64>,
    candidates: Option<i64>,
    article_count: i64,
    section_count: i64,
    total_words: String,
    reading_minutes: i64,
    cost_usd: Option<String>,
    generator_version: String,
}

#[derive(Template)]
#[template(path = "issue_full.html")]
struct IssueFullTemplate {
    page: Page,
    toc: Toc,
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
    toc: Toc,
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
    toc: Toc,
    display_date: Option<String>,
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
    toc: Toc,
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
    let issue_href = issue_href(date);
    let toc = issue_toc(&view, TocPosition::FrontPage);
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
                    is_lead: pick.is_lead,
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
    let colophon = colophon_view(&view.issue, view.legacy_counts.as_ref());
    let active_nav = if view.is_latest { "latest" } else { "archive" };
    let mut page = Page::new(format!("Issue {date}"), Some(viewer), active_nav);
    page.flash = take_flash(session).await?;
    Ok(Html(IssueFullTemplate {
        page,
        toc,
        display_date: view.issue.meta.display_date.clone(),
        issue_number: view.issue.meta.issue_number,
        stats_line: view.issue.meta.stats_line(),
        front_page_html: view.issue.editorial.front_page_html.clone(),
        downloads: view.downloads,
        sections,
        has_world: view.issue.world_briefing.is_some() || view.world_html.is_some(),
        has_behind: view.has_behind,
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
    let toc = issue_toc(&view, TocPosition::Article(article_id));
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
    let active_nav = if view.is_latest { "latest" } else { "archive" };
    let mut page = Page::new(article.title.clone(), Some(viewer.clone()), active_nav);
    page.flash = take_flash(&session).await?;
    Ok(Html(ArticleTemplate {
        page,
        toc,
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
        issue_href: issue_href(date),
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
    let toc = issue_toc(&view, TocPosition::World);
    let (display_date, body_html) = if let Some(briefing) = view.issue.world_briefing {
        (
            Some(display_date(briefing.date)),
            crate::world::render_xhtml(&briefing),
        )
    } else if let Some(body_html) = view.world_html {
        (None, body_html)
    } else {
        return Err(WebError::NotFound);
    };
    let active_nav = if view.is_latest { "latest" } else { "archive" };
    let mut page = Page::new("World Briefing", Some(viewer), active_nav);
    page.flash = take_flash(&session).await?;
    Ok(Html(WorldTemplate {
        page,
        toc,
        display_date,
        body_html,
        issue_href: issue_href(date),
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
    if !view.has_behind {
        return Err(WebError::NotFound);
    }
    let toc = issue_toc(&view, TocPosition::Behind);
    let behind = &view.issue.behind;
    let active_nav = if view.is_latest { "latest" } else { "archive" };
    let mut page = Page::new("Behind the paper", Some(viewer), active_nav);
    page.flash = take_flash(&session).await?;
    Ok(Html(BehindTemplate {
        page,
        toc,
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
        issue_href: issue_href(date),
    })
    .into_response())
}

fn article_href(date: Date, article_id: ArticleId) -> String {
    format!("/issues/{date}/articles/{article_id}")
}

fn issue_href(date: Date) -> String {
    format!("/issues/{date}")
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

fn colophon_view(issue: &Issue, legacy_counts: Option<&LegacyColophonCounts>) -> ColophonView {
    let colophon = &issue.colophon;
    let entries_fetched = legacy_counts
        .map(|counts| counts.entries_fetched)
        .unwrap_or(Some(colophon.entries_fetched));
    let feeds_seen = legacy_counts
        .map(|counts| counts.feeds_seen)
        .unwrap_or(Some(colophon.feeds_seen));
    let candidates = legacy_counts
        .map(|counts| counts.candidates)
        .unwrap_or(Some(colophon.candidates));
    let cost_usd = legacy_counts
        .map(|counts| counts.cost_usd)
        .unwrap_or(Some(colophon.cost_usd))
        .map(|cost| format!("${cost:.4}"));
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
        entries_fetched,
        feeds_seen,
        candidates,
        article_count: issue.meta.article_count,
        section_count: issue.meta.section_count,
        total_words: thousands(issue.meta.total_words),
        reading_minutes: issue.meta.reading_minutes,
        cost_usd,
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
        let epub_path = if with_json {
            None
        } else {
            Some(
                crate::epub::build_edition_with_images(
                    &issue,
                    Edition::Standard,
                    &crate::config::Config::default(),
                    dir.path(),
                    &[],
                )
                .unwrap()
                .path,
            )
        };
        let started_at = issue
            .meta
            .generated_at
            .checked_sub(jiff::Span::new().minutes(23))
            .unwrap();
        let run_id = db.start_run(issue.meta.date, started_at).await.unwrap();
        let mut report = crate::report::RunReport::new(issue.meta.date, started_at);
        report.counts.entries_fetched = issue.colophon.entries_fetched;
        report.counts.feeds_seen = issue.colophon.feeds_seen;
        report.counts.articles = issue.behind.considered;
        report.counts.eligible = issue.behind.eligible;
        report.counts.triaged = issue.behind.triaged;
        report.counts.assessed = issue.behind.read_closely;
        report.counts.shortlisted = issue.behind.shortlisted;
        report.counts.candidates = issue.colophon.candidates;
        report.counts.selected = issue.meta.article_count;
        report.counts.admitted_by = issue.behind.admitted_by.clone();
        report.counts.rated_with_embeddings = issue.behind.rated_with_embeddings;
        report.counts.knn_gate = issue.behind.knn_gate;
        report.counts.feed_gate = issue.behind.feed_gate;
        report.counts.embedded = 300;
        report.provider_costs = issue
            .colophon
            .provider_costs
            .iter()
            .map(|(provider, cost_usd)| {
                (
                    provider.clone(),
                    crate::report::ProviderUsage {
                        usage: crate::types::TokenUsage::default(),
                        cost_usd: *cost_usd,
                    },
                )
            })
            .collect();
        report.config_json = json!({
            "models": {
                "bulk": issue.colophon.models.bulk,
                "editor": issue.colophon.models.editor,
                "embedding": issue.behind.embedding_model,
            },
            "editorial": {"summary_model": "editor"},
            "voyage": {"model": issue.behind.embedding_model},
        });
        report.finish(issue.meta.generated_at);
        db.finish_run(run_id, &report).await.unwrap();

        if !with_json {
            let mut article = crate::epub::fixtures::article(0, 3001, "Legacy near miss");
            article.feed_title = "Near Misses Weekly".into();
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
            let article_id = db.upsert_article(&article).await.unwrap();
            crate::curate::telemetry::write(
                &db,
                &crate::curate::telemetry::CandidateRun {
                    run_id,
                    article_id,
                    stage: "shortlisted",
                    excluded_reason: Some("not_selected"),
                    admitted_by: Some("[\"blend\"]"),
                    signals_json: r#"{"v":1,"raw":{"quality":8.2,"fit":7.4}}"#,
                    utility: Some(81.0),
                    rank_utility: Some(3),
                    cluster_id: Some(1),
                    cluster_rank: Some(2),
                    editor_why: None,
                },
            )
            .await
            .unwrap();
        }
        let report_json = serde_json::to_string(&report).unwrap();
        let epub_path = epub_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        db.upsert_issue(
            issue.meta.date,
            issue.meta.issue_number,
            issue.meta.generated_at,
            epub_path.as_deref(),
            None,
            None,
            Some(&issue.editorial.front_page_html),
            Some(&report_json),
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
        assert!(loaded.world_html.as_deref().is_some_and(|html| {
            html.contains("Something happened somewhere") && !html.contains("<h1>World Briefing")
        }));
        assert_eq!(loaded.issue.colophon.entries_fetched, 431);
        assert_eq!(loaded.issue.colophon.feeds_seen, 92);
        assert_eq!(loaded.issue.colophon.candidates, 120);
        assert_eq!(loaded.issue.colophon.models.bulk, "deepseek-v4-flash");
        assert_eq!(loaded.issue.colophon.models.editor, "claude-opus-5");
        assert_eq!(loaded.issue.colophon.models.summaries, "claude-opus-5");
        assert_eq!(loaded.issue.behind.near_misses[0].title, "Legacy near miss");
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
    async fn issue_navigation_marks_only_the_newest_issue_as_latest() {
        let (_dir, db, source) = seeded_issue(true).await;
        let old_date = source.meta.date;
        let latest_date: Date = "2026-08-16".parse().unwrap();
        let mut latest = source.clone();
        latest.meta.date = latest_date;
        latest.meta.issue_number += 1;
        latest.meta.display_date = display_date(latest_date);
        latest.lineup.date = latest_date;
        if let Some(world) = &mut latest.world_briefing {
            world.date = latest_date;
        }
        let latest_json = serde_json::to_string(&latest).unwrap();
        db.upsert_issue(
            latest_date,
            latest.meta.issue_number,
            latest.meta.generated_at,
            None,
            None,
            None,
            Some(&latest.editorial.front_page_html),
            None,
            Some(&latest_json),
        )
        .await
        .unwrap();
        db.replace_issue_articles(latest_date, &latest.lineup.picks)
            .await
            .unwrap();

        let old_view = load(&db, &crate::config::Config::default(), old_date)
            .await
            .unwrap()
            .unwrap();
        let latest_view = load(&db, &crate::config::Config::default(), latest_date)
            .await
            .unwrap()
            .unwrap();
        assert!(!old_view.is_latest);
        assert!(latest_view.is_latest);

        let app = crate::server::router(crate::server::AppState::new(
            db,
            crate::config::Config::default(),
            None,
        ));
        let old = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{old_date}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::OK);
        let old = response_text(old).await;
        assert!(
            old.contains("href=\"/issues\" aria-current=\"page\">Archive</a>"),
            "{old}"
        );
        assert!(!old.contains("href=\"/\" aria-current=\"page\">Latest</a>"));

        let latest = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{latest_date}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(latest.status(), StatusCode::OK);
        let latest = response_text(latest).await;
        assert!(
            latest.contains("href=\"/\" aria-current=\"page\">Latest</a>"),
            "{latest}"
        );
        assert!(!latest.contains("href=\"/issues\" aria-current=\"page\">Archive</a>"));
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
        assert!(html.contains("What it argues, and why it is worth the time."));
        assert!(html.contains("Why it's here"));
        assert!(!html.contains("Two stories today"));
        assert!(!html.contains("Something happened"));
        assert!(!html.contains("Body of"));
        assert!(!html.contains("write path"));
        // The table of contents is a signed-in feature.
        assert!(!html.contains("data-toc-toggle"));
        assert!(!html.contains("id=\"toc\""));

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
        assert_eq!(
            archive.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
        );

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
        assert_eq!(
            feed.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
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
        assert!(feed.contains("What it argues, and why it is worth the time."));
        assert!(feed.contains("Why it&apos;s here"));
        assert!(!feed.contains("Something happened"));
        assert!(!feed.contains("Two stories today"));
        assert!(!feed.contains("Body of"));
        assert!(!feed.contains("write path"));

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
        assert_eq!(
            robots.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=86400"
        );
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
        assert_eq!(
            reports.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
        );
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
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(response_text(missing).await.contains("Not found"));
    }

    #[tokio::test]
    async fn fallback_full_issue_recovers_colophon_world_and_behind() {
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
        assert!(issue.contains(&format!("/issues/{}/world", source.meta.date)));
        assert!(issue.contains(&format!("/issues/{}/behind", source.meta.date)));
        assert!(issue.contains("431 from 92 feeds"));
        // The recovered chapters are offered by the sidebar as well as the page.
        assert!(issue.contains("data-toc-toggle"));
        assert_eq!(
            issue
                .matches(&format!("/issues/{}/world", source.meta.date))
                .count(),
            2
        );
        assert!(issue.contains("Front page"));
        assert!(issue.contains("deepseek-v4-flash"));
        assert!(issue.contains("claude-opus-5"));
        assert!(!issue.contains("0 from 0 feeds"));

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
        let world = response_text(world).await;
        assert!(world.contains("Something happened somewhere"));
        assert!(world.matches("World Briefing").count() >= 2); // page title + chapter heading

        let behind = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/behind", source.meta.date))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(behind.status(), StatusCode::OK);
        let behind = response_text(behind).await;
        assert!(behind.contains("Considered 412 articles"));
        assert!(behind.contains("Legacy near miss"));
    }

    #[tokio::test]
    async fn fallback_uses_finished_run_columns_and_marks_unknown_counts_na() {
        let (_dir, db, source) = seeded_issue(false).await;
        sqlx::query(
            "UPDATE issues SET report_json = NULL, epub_path = '/missing/legacy.epub'
             WHERE date = ?",
        )
        .bind(source.meta.date.to_string())
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE runs SET report_json = NULL WHERE date = ?")
            .bind(source.meta.date.to_string())
            .execute(db.pool())
            .await
            .unwrap();
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
        assert!(issue.contains("<dd>431</dd>"));
        assert!(!issue.contains("n/a feeds"));
        assert!(issue.contains("120"));
        assert!(!issue.contains("0 from 0 feeds"));
        assert!(issue.contains(&format!("/issues/{}/behind", source.meta.date)));
        // No EPUB to recover the World Briefing from: the sidebar must not offer it.
        assert!(!issue.contains(&format!("/issues/{}/world", source.meta.date)));
        assert!(issue.contains("Behind the paper"));

        let behind = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{}/behind", source.meta.date))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(behind.status(), StatusCode::OK);
        assert!(response_text(behind).await.contains("Legacy near miss"));
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
        std::fs::write(&standard, vec![0; 2 * 1024]).unwrap();
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
        assert!(issue.contains("2.0 KB"));
        assert!(!issue.contains("2048 bytes"));
        assert!(!issue.contains("Download X4 EPUB"));
        assert!(!issue.contains("Download XTC"));
    }

    #[test]
    fn file_sizes_are_human_readable() {
        assert_eq!(format_file_size(42), "42 B");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(5 * 1024 * 1024), "5.0 MB");
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

    #[tokio::test]
    async fn toc_numbers_chapters_across_sections_and_tracks_the_reader() {
        let (_dir, db, source) = seeded_issue(true).await;
        let view = load(&db, &crate::config::Config::default(), source.meta.date)
            .await
            .unwrap()
            .unwrap();
        let front = issue_toc(&view, TocPosition::FrontPage);
        let shape: Vec<(TocKind, Option<usize>, &str)> = front
            .items
            .iter()
            .map(|item| (item.kind, item.number, item.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (TocKind::Brief, None, "The Brief"),
                (TocKind::Section, None, "Top Stories"),
                (TocKind::Chapter, Some(1), "The Lead Story"),
                (TocKind::Section, None, "Niche Corner"),
                (TocKind::Chapter, Some(2), "A Niche Delight & Other Tales"),
                (TocKind::World, None, "World Briefing"),
                (TocKind::Behind, None, "Behind the paper"),
                (TocKind::Colophon, None, "Colophon"),
            ]
        );
        // Two articles plus the World Briefing and Behind the paper chapters.
        assert_eq!(front.total, 4);
        assert_eq!(front.position, 0);
        assert_eq!(front.current_label(), "The Brief");
        assert_eq!(front.short_date, "Sat, Aug 15");
        assert_eq!(
            front
                .items
                .iter()
                .filter(|item| !item.is_section())
                .map(|item| item.progress)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 4]
        );
        // Exactly one hairline, above the first back-matter entry.
        assert_eq!(
            front
                .items
                .iter()
                .filter(|item| item.divider)
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["World Briefing"]
        );
        assert!(
            front.items[2].minutes.is_some() && front.items[2].href.contains("/articles/"),
            "chapters link to their article and carry a read time"
        );

        let second = issue_toc(
            &view,
            TocPosition::Article(view.issue.lineup.picks[1].article.id),
        );
        assert_eq!(second.position, 2);
        assert_eq!(second.current_label(), "A Niche Delight & Other Tales");
        assert_eq!(second.items.iter().filter(|item| item.current).count(), 1);
        assert_eq!(issue_toc(&view, TocPosition::World).position, 3);
        assert_eq!(issue_toc(&view, TocPosition::Behind).position, 4);
    }

    #[tokio::test]
    async fn the_sidebar_marks_the_current_chapter_on_every_signed_in_issue_page() {
        let (_dir, db, source) = seeded_issue(true).await;
        crate::web::users::add(&db, "reader", "correct horse battery", false)
            .await
            .unwrap();
        let app = crate::server::router(crate::server::AppState::new(
            db,
            crate::config::Config::default(),
            None,
        ));
        let cookie = login_cookie(&app, "reader", "correct horse battery").await;
        let date = source.meta.date;
        let article_id = source.lineup.picks[1].article.id;
        let cases = [
            (format!("/issues/{date}"), "Front page", issue_href(date)),
            (
                article_href(date, article_id),
                "Chapter 2 of 4",
                article_href(date, article_id),
            ),
            (
                format!("/issues/{date}/world"),
                "Chapter 3 of 4",
                format!("/issues/{date}/world"),
            ),
            (
                format!("/issues/{date}/behind"),
                "Chapter 4 of 4",
                format!("/issues/{date}/behind"),
            ),
        ];
        for (path, progress, current) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&path)
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let html = response_text(response).await;
            assert!(html.contains("data-toc-toggle"), "{path} has no toc bar");
            assert!(html.contains("id=\"toc\""), "{path} has no toc panel");
            assert!(
                html.contains("data-toc-status"),
                "{path} has no live status"
            );
            assert!(html.contains(progress), "{path} is missing {progress:?}");
            let current_link = html
                .split(&format!("class=\"toc-link\" href=\"{current}\""))
                .nth(1)
                .and_then(|rest| rest.split("</a>").next());
            assert!(
                current_link.is_some_and(|link| link.contains("aria-current=\"page\"")),
                "{path} does not mark {current} as current"
            );
            assert!(html.contains(&format!("{}#colophon", issue_href(date))));
            assert!(html.contains("A Niche Delight &#38; Other Tales"));
            if path == issue_href(date) {
                assert!(html.contains(&format!(
                    "data-toc-entry=\"{}\"",
                    article_href(date, source.lineup.picks[0].article.id)
                )));
                assert!(
                    html.contains(&format!("data-toc-entry=\"{}#colophon\"", issue_href(date)))
                );
            }
        }

        let anonymous = app
            .oneshot(
                Request::builder()
                    .uri(format!("/issues/{date}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::OK);
        assert!(!response_text(anonymous).await.contains("data-toc-toggle"));
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
