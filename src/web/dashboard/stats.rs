//! Dashboard: the stats page (`/dashboard/stats`, dashboard plan §12) and the
//! server-rendered SVG sparkline the overview shares (§9.1).
//!
//! The figures come from `telemetry::stats_data`, the same source as
//! `daily-epub stats`; the page shows them as tables plus three sparklines
//! (cost per day stacked per provider, selected per issue, ratings per week by
//! label) and the retriever yield with its up/down ratio.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use askama::Template;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_login::tower_sessions::Session;
use jiff::Timestamp;
use serde::Deserialize;

use crate::curate::telemetry::{self, StatsData};
use crate::report::RunReport;
use crate::server::AppState;
use crate::web::session::{AuthSession, Viewer};
use crate::web::{Html, Page, WebError, take_flash};

use super::fmt_usd;

/// Routes contributed by this page group (merged by `dashboard::router`).
pub fn routes() -> Router<AppState> {
    Router::new().route("/dashboard/stats", get(stats))
}

/// The windows the page offers; anything else falls back to the first.
pub const WINDOWS: [i64; 3] = [14, 30, 90];

// ---------------------------------------------------------------------------
// Sparklines (§9.1, §12)
// ---------------------------------------------------------------------------

const SPARK_WIDTH: f64 = 240.0;
const SPARK_HEIGHT: f64 = 48.0;
const SPARK_PAD: f64 = 2.0;
/// Fill classes cycle through this many series colours (`.spark .s0` …).
pub const SERIES_CLASSES: usize = 6;

/// One stacked-bar segment: pre-formatted SVG coordinates, a series index for
/// its fill class and a `<title>` label.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub x: String,
    pub y: String,
    pub w: String,
    pub h: String,
    pub series: usize,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Legend {
    pub name: String,
    pub series: usize,
}

/// A small SVG chart rendered by `dashboard/_sparkline.html` from numbers
/// only — presentation attributes and classes, no inline styles (the CSP is
/// `style-src 'self'`) and no `|safe`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sparkline {
    pub title: String,
    pub caption: String,
    pub width: u32,
    pub height: u32,
    /// `<polyline points>` of a line chart; empty for a bar chart.
    pub points: String,
    pub bars: Vec<Bar>,
    pub legend: Vec<Legend>,
    pub first_label: String,
    pub last_label: String,
    pub empty: bool,
}

fn coord(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
}

impl Sparkline {
    fn blank(title: &str) -> Self {
        Self {
            title: title.to_string(),
            caption: String::new(),
            width: SPARK_WIDTH as u32,
            height: SPARK_HEIGHT as u32,
            points: String::new(),
            bars: Vec::new(),
            legend: Vec::new(),
            first_label: String::new(),
            last_label: String::new(),
            empty: true,
        }
    }

    /// A line over `values` (baseline 0), labelled with the first and last
    /// point's label; the caption reports the count and the maximum.
    pub fn line(
        title: &str,
        values: &[f64],
        labels: (&str, &str),
        unit: &str,
        format_max: impl Fn(f64) -> String,
    ) -> Self {
        let mut spark = Self::blank(title);
        if values.is_empty() {
            return spark;
        }
        let max = values.iter().copied().fold(0.0_f64, f64::max);
        let inner_w = SPARK_WIDTH - 2.0 * SPARK_PAD;
        let inner_h = SPARK_HEIGHT - 2.0 * SPARK_PAD;
        let step = if values.len() > 1 {
            inner_w / (values.len() - 1) as f64
        } else {
            0.0
        };
        let mut points = String::new();
        for (index, value) in values.iter().enumerate() {
            let x = if values.len() > 1 {
                SPARK_PAD + step * index as f64
            } else {
                SPARK_WIDTH / 2.0
            };
            let y = if max > 0.0 {
                SPARK_HEIGHT - SPARK_PAD - value.max(0.0) / max * inner_h
            } else {
                SPARK_HEIGHT - SPARK_PAD
            };
            if !points.is_empty() {
                points.push(' ');
            }
            let _ = write!(points, "{},{}", coord(x), coord(y));
        }
        spark.points = points;
        spark.caption = format!("{} {unit} · max {}", values.len(), format_max(max));
        spark.first_label = labels.0.to_string();
        spark.last_label = labels.1.to_string();
        spark.empty = false;
        spark
    }

    /// Stacked bars: one column per `(label, segments)` where each segment is
    /// `(series name, value)`; series get fill classes in first-seen order.
    pub fn stacked(
        title: &str,
        columns: &[(String, Vec<(String, f64)>)],
        unit: &str,
        format_max: impl Fn(f64) -> String,
    ) -> Self {
        let mut spark = Self::blank(title);
        if columns.is_empty() {
            return spark;
        }
        let mut series: Vec<String> = Vec::new();
        for (_, segments) in columns {
            for (name, _) in segments {
                if !series.contains(name) {
                    series.push(name.clone());
                }
            }
        }
        let max = columns
            .iter()
            .map(|(_, segments)| segments.iter().map(|(_, v)| v.max(0.0)).sum::<f64>())
            .fold(0.0_f64, f64::max);
        let inner_w = SPARK_WIDTH - 2.0 * SPARK_PAD;
        let inner_h = SPARK_HEIGHT - 2.0 * SPARK_PAD;
        let slot = inner_w / columns.len() as f64;
        let gap = if slot > 3.0 { 1.0 } else { 0.0 };
        let bar_w = (slot - gap).max(0.5);
        let mut bars = Vec::new();
        for (index, (label, segments)) in columns.iter().enumerate() {
            let x = SPARK_PAD + slot * index as f64;
            let mut top = SPARK_HEIGHT - SPARK_PAD;
            for (name, value) in segments {
                let value = value.max(0.0);
                if value <= 0.0 || max <= 0.0 {
                    continue;
                }
                let h = value / max * inner_h;
                top -= h;
                let series_index = series.iter().position(|s| s == name).unwrap_or(0);
                bars.push(Bar {
                    x: coord(x),
                    y: coord(top),
                    w: coord(bar_w),
                    h: coord(h),
                    series: series_index % SERIES_CLASSES,
                    label: format!("{label} · {name}: {}", format_max(value)),
                });
            }
        }
        spark.bars = bars;
        spark.legend = series
            .iter()
            .enumerate()
            .map(|(index, name)| Legend {
                name: name.clone(),
                series: index % SERIES_CLASSES,
            })
            .collect();
        spark.caption = format!("{} {unit} · max {}", columns.len(), format_max(max));
        spark.first_label = columns[0].0.clone();
        spark.last_label = columns[columns.len() - 1].0.clone();
        spark.empty = false;
        spark
    }
}

// ---------------------------------------------------------------------------
// Stats page (§12)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    days: Option<i64>,
}

#[derive(Debug, Clone)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Debug, Clone)]
struct RetrieverLine {
    retriever: String,
    rated: i64,
    up: i64,
    down: i64,
    ratio: String,
}

#[derive(Debug, Clone)]
struct CostRow {
    date: String,
    cells: Vec<String>,
    total: String,
}

#[derive(Debug, Clone)]
struct RunLine {
    run_id: i64,
    date: String,
    status: String,
    cost: String,
    selected: i64,
    duration: String,
}

#[derive(Template)]
#[template(path = "dashboard/stats.html")]
struct StatsTemplate {
    page: Page,
    days: i64,
    windows: Vec<i64>,
    since_date: String,
    today: String,
    summary: Vec<KeyValue>,
    ratings: Vec<KeyValue>,
    retrievers: Vec<RetrieverLine>,
    exploration: Vec<KeyValue>,
    providers: Vec<String>,
    cost_rows: Vec<CostRow>,
    cost_per_day: Vec<KeyValue>,
    runs: Vec<RunLine>,
    sparklines: Vec<Sparkline>,
    text: String,
}

/// `?days=` clamped to one of [`WINDOWS`].
pub fn window(days: Option<i64>) -> i64 {
    days.filter(|days| WINDOWS.contains(days))
        .unwrap_or(WINDOWS[0])
}

/// `GET /dashboard/stats?days=14|30|90`.
async fn stats(
    State(state): State<AppState>,
    auth: AuthSession,
    Extension(session): Extension<Session>,
    Query(query): Query<StatsQuery>,
) -> Result<Response, WebError> {
    let viewer = auth.user().await.map(Viewer::from);
    let days = window(query.days);
    let now = Timestamp::now();
    let data = telemetry::stats_data(&state.db, days, now)
        .await
        .map_err(WebError::Internal)?;
    let text = telemetry::render_stats_text(&data);

    let summary = vec![
        KeyValue {
            key: "Issues".into(),
            value: data.issues.to_string(),
        },
        KeyValue {
            key: "Articles published".into(),
            value: data.published.to_string(),
        },
        KeyValue {
            key: "Mean issue size".into(),
            value: format!("{} articles", data.mean_issue_size()),
        },
        KeyValue {
            key: "Explicit ratings".into(),
            value: data.total_ratings.to_string(),
        },
        KeyValue {
            key: "Ratings per issue".into(),
            value: data.ratings_per_issue(),
        },
        KeyValue {
            key: "Mean generation time".into(),
            value: match data.mean_generation_secs() {
                Some(secs) => format!(
                    "{} ({} runs)",
                    RunReport::format_duration(secs),
                    data.durations.len()
                ),
                None => "n/a (0 runs)".into(),
            },
        },
    ];
    let ratings = data
        .ratings_by_label
        .iter()
        .map(|(label, n)| KeyValue {
            key: label.clone(),
            value: n.to_string(),
        })
        .collect();
    let retrievers = data
        .per_retriever
        .iter()
        .map(|(retriever, counts)| RetrieverLine {
            retriever: retriever.clone(),
            rated: counts.rated,
            up: counts.up,
            down: counts.down,
            ratio: counts.ratio(),
        })
        .collect();
    let exploration = vec![
        KeyValue {
            key: "Admitted".into(),
            value: data.exploration_admitted.to_string(),
        },
        KeyValue {
            key: "Selected".into(),
            value: data.exploration_selected.to_string(),
        },
        KeyValue {
            key: "Rated positively".into(),
            value: data.exploration_positive.to_string(),
        },
    ];
    let providers: Vec<String> = data.provider_totals.keys().cloned().collect();
    let cost_rows = data
        .cost_by_day
        .iter()
        .map(|(date, by_provider)| CostRow {
            date: date.clone(),
            cells: providers
                .iter()
                .map(|provider| {
                    by_provider
                        .get(provider)
                        .map(|usd| format!("{usd:.3}"))
                        .unwrap_or_else(|| "—".into())
                })
                .collect(),
            total: format!("{:.3}", by_provider.values().sum::<f64>()),
        })
        .collect();
    let mut cost_per_day: Vec<KeyValue> = data
        .provider_totals
        .iter()
        .map(|(provider, total)| KeyValue {
            key: provider.clone(),
            value: format!("${:.3}", data.per_day(*total)),
        })
        .collect();
    cost_per_day.push(KeyValue {
        key: "total".into(),
        value: format!("${:.3}", data.per_day(data.grand_total())),
    });
    let runs = data
        .runs
        .iter()
        .map(|run| RunLine {
            run_id: run.run_id,
            date: run.date.clone(),
            status: run.status.clone(),
            cost: fmt_usd(run.cost_usd),
            selected: run.selected,
            duration: super::fmt_duration(run.duration_secs),
        })
        .collect();

    let mut page = Page::new("Stats", viewer, "dashboard");
    page.flash = take_flash(&session).await?;
    Ok(Html(StatsTemplate {
        page,
        days,
        windows: WINDOWS.to_vec(),
        since_date: data.since_date.clone(),
        today: data.today.clone(),
        summary,
        ratings,
        retrievers,
        exploration,
        providers,
        cost_rows,
        cost_per_day,
        runs,
        sparklines: stats_sparklines(&data),
        text,
    })
    .into_response())
}

/// Every UTC date from `since` to `today` inclusive.
fn dates_between(since: &str, today: &str) -> Vec<String> {
    let (Ok(mut date), Ok(end)) = (
        since.parse::<jiff::civil::Date>(),
        today.parse::<jiff::civil::Date>(),
    ) else {
        return Vec::new();
    };
    let mut dates = Vec::new();
    while date <= end && dates.len() < 400 {
        dates.push(date.to_string());
        match date.tomorrow() {
            Ok(next) => date = next,
            Err(_) => break,
        }
    }
    dates
}

/// The stats page's three charts (§12).
fn stats_sparklines(data: &StatsData) -> Vec<Sparkline> {
    let providers: Vec<String> = data.provider_totals.keys().cloned().collect();
    let empty = BTreeMap::new();
    let cost_columns: Vec<(String, Vec<(String, f64)>)> =
        dates_between(&data.since_date, &data.today)
            .into_iter()
            .map(|date| {
                let by_provider = data.cost_by_day.get(&date).unwrap_or(&empty);
                let segments = providers
                    .iter()
                    .map(|provider| {
                        (
                            provider.clone(),
                            by_provider.get(provider).copied().unwrap_or(0.0),
                        )
                    })
                    .collect();
                (date, segments)
            })
            .collect();
    let cost = if data.cost_by_day.is_empty() {
        Sparkline::stacked("Cost per day", &[], "days", fmt_usd)
    } else {
        Sparkline::stacked("Cost per day", &cost_columns, "days", |usd| {
            format!("${usd:.3}")
        })
    };

    let selected_values: Vec<f64> = data
        .selected_per_issue
        .iter()
        .map(|(_, n)| *n as f64)
        .collect();
    let selected = Sparkline::line(
        "Selected per issue",
        &selected_values,
        (
            data.selected_per_issue
                .first()
                .map(|(date, _)| date.as_str())
                .unwrap_or(""),
            data.selected_per_issue
                .last()
                .map(|(date, _)| date.as_str())
                .unwrap_or(""),
        ),
        "issues",
        |n| format!("{n:.0}"),
    );

    let week_columns: Vec<(String, Vec<(String, f64)>)> = data
        .ratings_per_week
        .iter()
        .map(|(week, by_label)| {
            (
                week.clone(),
                by_label
                    .iter()
                    .map(|(label, n)| (label.clone(), *n as f64))
                    .collect(),
            )
        })
        .collect();
    let weekly = Sparkline::stacked("Ratings per week", &week_columns, "weeks", |n| {
        format!("{n:.0}")
    });
    vec![cost, selected, weekly]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::dashboard::tests::{
        app_with_users, assert_admin_only, get, login_cookie, response_text, seed,
    };

    #[test]
    fn window_falls_back_to_fourteen_days() {
        assert_eq!(window(None), 14);
        assert_eq!(window(Some(30)), 30);
        assert_eq!(window(Some(90)), 90);
        assert_eq!(window(Some(7)), 14);
        assert_eq!(window(Some(-1)), 14);
    }

    #[test]
    fn line_sparkline_scales_to_the_maximum_and_marks_empties() {
        let spark = Sparkline::line(
            "Cost",
            &[0.0, 2.0, 1.0],
            ("2026-09-01", "2026-09-03"),
            "runs",
            fmt_usd,
        );
        assert!(!spark.empty);
        assert_eq!(spark.points, "2,46 120,2 238,24");
        assert_eq!(spark.caption, "3 runs · max $2.00");
        assert_eq!(spark.first_label, "2026-09-01");
        assert_eq!(spark.last_label, "2026-09-03");
        assert!(spark.bars.is_empty());

        let flat = Sparkline::line("Flat", &[0.0, 0.0], ("a", "b"), "runs", fmt_usd);
        assert_eq!(flat.points, "2,46 238,46");

        let none = Sparkline::line("None", &[], ("", ""), "runs", fmt_usd);
        assert!(none.empty);
        assert!(none.points.is_empty());
    }

    #[test]
    fn stacked_sparkline_assigns_series_classes_in_first_seen_order() {
        let columns = vec![
            (
                "2026-09-01".to_string(),
                vec![("deepseek".to_string(), 0.1), ("voyage".to_string(), 0.1)],
            ),
            (
                "2026-09-02".to_string(),
                vec![("deepseek".to_string(), 0.0), ("voyage".to_string(), 0.4)],
            ),
        ];
        let spark =
            Sparkline::stacked("Cost per day", &columns, "days", |usd| format!("${usd:.3}"));
        assert!(!spark.empty);
        assert_eq!(spark.legend.len(), 2);
        assert_eq!(spark.legend[0].name, "deepseek");
        assert_eq!(spark.legend[0].series, 0);
        assert_eq!(spark.legend[1].series, 1);
        // Three visible segments: the zero-valued one is skipped.
        assert_eq!(spark.bars.len(), 3);
        assert_eq!(spark.bars[0].series, 0);
        assert_eq!(spark.bars[0].h, "11");
        assert_eq!(spark.bars[0].y, "35");
        assert_eq!(spark.bars[1].y, "24", "stacked on top of the first");
        assert_eq!(spark.bars[2].h, "44");
        assert_eq!(spark.bars[2].label, "2026-09-02 · voyage: $0.400");
        assert_eq!(spark.caption, "2 days · max $0.400");

        let none = Sparkline::stacked("None", &[], "days", fmt_usd);
        assert!(none.empty);
    }

    #[test]
    fn dates_between_is_inclusive() {
        assert_eq!(
            dates_between("2026-08-30", "2026-09-01"),
            vec!["2026-08-30", "2026-08-31", "2026-09-01"]
        );
        assert!(dates_between("2026-09-02", "2026-09-01").is_empty());
        assert!(dates_between("bad", "2026-09-01").is_empty());
    }

    #[tokio::test]
    async fn stats_page_shows_tables_sparklines_and_the_text() {
        let seed = seed().await;
        // The seed is dated 2026-09-02; the page windows on `now`, so move the
        // runs and rating events into the last hour to keep the test stable.
        let now = Timestamp::now();
        let earlier = crate::db::fmt_ts(now - jiff::Span::new().hours(1));
        sqlx::query("UPDATE runs SET started_at = ?, finished_at = ?")
            .bind(&earlier)
            .bind(crate::db::fmt_ts(now))
            .execute(seed.db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE rating_events SET event_at = ?")
            .bind(&earlier)
            .execute(seed.db.pool())
            .await
            .unwrap();
        let app = app_with_users(&seed.db).await;
        let body = assert_admin_only(&app, "/dashboard/stats").await;
        assert!(body.contains("?days=30"), "{body}");
        assert!(body.contains("?days=90"), "{body}");
        assert!(body.contains("Retriever yield"), "{body}");
        assert!(body.contains("<rect"), "{body}");
        assert!(body.contains("Ratings per week"), "{body}");
        assert!(body.contains("badge loved"), "{body}");
        assert!(body.contains("deepseek"), "{body}");
        assert!(
            body.contains("mean generation time:"),
            "the CLI text is included"
        );
        assert!(!body.contains("style=\""), "no inline styles under the CSP");
        assert!(
            body.contains(&format!("/dashboard/runs/{}", seed.run_id)),
            "{body}"
        );

        // An out-of-catalogue window falls back to 14 days.
        let admin = login_cookie(&app, "admin", "correct horse battery").await;
        let fallback = get(&app, "/dashboard/stats?days=7", Some(&admin)).await;
        assert_eq!(fallback.status(), axum::http::StatusCode::OK);
        let body = response_text(fallback).await;
        assert!(body.contains("stats: last 14 days"), "{body}");
    }
}
