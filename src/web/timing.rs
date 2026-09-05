//! Per-request `Server-Timing` metrics (§3.12).
//!
//! The header splits one request into the parts worth telling apart:
//!
//! | metric | what it covers |
//! |---|---|
//! | `app` | everything this process did, the outermost middleware inwards |
//! | `sess` | the session/auth layers: the cookie's `SELECT` on the way in, the save on the way out |
//! | `db` | SQLite statement time, with the statement count in `desc` |
//! | `tpl` | askama rendering |
//!
//! They overlap rather than partition: `sess` and `db` are both inside `app`,
//! and they share the session's own statements. Handler time is `app` − `sess`,
//! and `sess` − (the session's share of `db`) is what the auth layers cost
//! beyond SQLite — on a warm `/account` that gap has been most of the request,
//! which is the sort of thing these metrics exist to make visible.
//!
//! `db` is collected the only way that works for SQLite: sqlx runs statements
//! on a per-connection worker *thread*, so a task-local set by the middleware
//! is invisible there. What does cross over is the caller's [`tracing::Span`] —
//! `sqlx-sqlite` sends it alongside every command and enters it while the
//! statement runs — so the `sqlx::query` event it emits on finishing lands
//! inside this request's span. [`SqlxTimingLayer`] picks the elapsed time off
//! that event and adds it to the metrics hanging in the span's extensions.
//! Metrics recorded on the request task itself (`tpl`, the `sess` boundary)
//! take the shorter path through the task-local.
//!
//! Nothing here is load-bearing: without [`SqlxTimingLayer`] installed the `db`
//! metric is simply absent, and without a subscriber at all the span costs
//! nothing and `app` still reports.
//!
//! What installing it does cost is process-wide: the filter's `DEBUG` hint
//! lifts the global maximum level from `INFO`, so every `DEBUG` span in any
//! dependency (tower-http's per-request span, axum-login's instrumented user
//! loads) is now materialised in the registry, and sqlx builds a summary
//! string for each statement it reports. Microseconds per request, against
//! an origin measured in milliseconds; noted so it is not mistaken for free.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument as _;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Target of the per-request span. It is how [`SqlxTimingLayer`] recognises
/// the one span to hang metrics on, and being off the crate's module path also
/// keeps the span out of the log under the default `RUST_LOG` (`info`).
const REQUEST_TARGET: &str = "daily_epub::request";

/// The target sqlx logs every finished statement to.
const SQLX_TARGET: &str = "sqlx::query";

/// Sentinel for a boundary mark that was never reached.
const UNSET: u64 = u64::MAX;

tokio::task_local! {
    /// The metrics for the request running on this task. Set by
    /// [`server_timing`], so it covers everything inside that middleware.
    static CURRENT: Arc<RequestMetrics>;
}

/// One request's accumulated timings.
///
/// Written from two places at once — the request task and a sqlx worker
/// thread — so every counter is atomic. `Relaxed` is enough: nothing here
/// orders anything else, and the totals are only read after the response is
/// complete and both writers are done.
#[derive(Debug)]
pub struct RequestMetrics {
    started: Instant,
    db_nanos: AtomicU64,
    db_statements: AtomicU32,
    tpl_nanos: AtomicU64,
    tpl_renders: AtomicU32,
    /// Nanoseconds after `started` at which the routed part of the stack was
    /// entered and left; [`UNSET`] until [`route_boundary`] marks them.
    route_entered: AtomicU64,
    route_left: AtomicU64,
}

impl RequestMetrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            db_nanos: AtomicU64::new(0),
            db_statements: AtomicU32::new(0),
            tpl_nanos: AtomicU64::new(0),
            tpl_renders: AtomicU32::new(0),
            route_entered: AtomicU64::new(UNSET),
            route_left: AtomicU64::new(UNSET),
        }
    }

    fn add_db(&self, elapsed: Duration) {
        self.db_nanos.fetch_add(nanos(elapsed), Ordering::Relaxed);
        self.db_statements.fetch_add(1, Ordering::Relaxed);
    }

    fn add_render(&self, elapsed: Duration) {
        self.tpl_nanos.fetch_add(nanos(elapsed), Ordering::Relaxed);
        self.tpl_renders.fetch_add(1, Ordering::Relaxed);
    }

    /// The `sess` metric: everything outside the routed stack, which is the
    /// session load on the way in plus the session save on the way out. Absent
    /// until both boundaries have been marked — a request rejected before it
    /// reaches the router (a 425, a cross-origin 403) never gets there.
    fn session_nanos(&self) -> Option<u64> {
        let entered = self.route_entered.load(Ordering::Relaxed);
        let left = self.route_left.load(Ordering::Relaxed);
        if entered == UNSET || left == UNSET {
            return None;
        }
        Some(entered + nanos(self.started.elapsed()).saturating_sub(left))
    }

    /// The header value: `app` always, the rest only where they were measured.
    fn header(&self) -> String {
        let mut value = String::with_capacity(72);
        write!(
            value,
            "app;dur={:.2}",
            millis(nanos(self.started.elapsed()))
        )
        .expect("writing to a String cannot fail");
        if let Some(session) = self.session_nanos() {
            write!(value, ", sess;dur={:.2}", millis(session))
                .expect("writing to a String cannot fail");
        }
        let statements = self.db_statements.load(Ordering::Relaxed);
        if statements > 0 {
            let unit = if statements == 1 { "query" } else { "queries" };
            write!(
                value,
                ", db;dur={:.2};desc=\"{statements} {unit}\"",
                millis(self.db_nanos.load(Ordering::Relaxed))
            )
            .expect("writing to a String cannot fail");
        }
        if self.tpl_renders.load(Ordering::Relaxed) > 0 {
            write!(
                value,
                ", tpl;dur={:.2}",
                millis(self.tpl_nanos.load(Ordering::Relaxed))
            )
            .expect("writing to a String cannot fail");
        }
        value
    }
}

/// Saturating, because a `Duration` holds more nanoseconds than a `u64` can.
fn nanos(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}

/// Run `f` against the metrics of the request on this task, if there is one.
fn with_metrics(f: impl FnOnce(&RequestMetrics)) {
    let _ = CURRENT.try_with(|metrics| f(metrics));
}

/// Add one template render to the `tpl` metric. Called by
/// [`crate::web::Html`], which is how every HTML page is rendered.
pub fn record_render(elapsed: Duration) {
    with_metrics(|metrics| metrics.add_render(elapsed));
}

/// `Server-Timing` on every response: see the module docs for the metrics.
///
/// Outermost layer, so `app` covers the session load, auth and rendering. The
/// per-request span is opened *inside* the task-local scope so that
/// [`SqlxTimingLayer::on_new_span`] can hang these same metrics on it.
pub async fn server_timing(request: Request, next: Next) -> Response {
    let metrics = Arc::new(RequestMetrics::new());
    let mut response = CURRENT
        .scope(metrics.clone(), async move {
            let span = tracing::debug_span!(target: REQUEST_TARGET, "http_request");
            next.run(request).instrument(span).await
        })
        .await;
    if let Ok(value) = HeaderValue::from_str(&metrics.header()) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static("server-timing"), value);
    }
    response
}

/// Marks where the routed part of the stack begins, so the time spent in the
/// session and auth layers wrapped around it can be reported as `sess`.
/// Belongs immediately inside the auth layer; anywhere else and the metric
/// quietly measures something other than its name.
pub async fn route_boundary(request: Request, next: Next) -> Response {
    with_metrics(|metrics| {
        metrics
            .route_entered
            .store(nanos(metrics.started.elapsed()), Ordering::Relaxed);
    });
    let response = next.run(request).await;
    with_metrics(|metrics| {
        metrics
            .route_left
            .store(nanos(metrics.started.elapsed()), Ordering::Relaxed);
    });
    response
}

/// Adds every `sqlx::query` event's elapsed time to the request it ran for.
///
/// Install it with [`sqlx_timing_layer`], which filters it down to the two
/// targets it needs; on its own it would be asked about every event in the
/// process.
pub struct SqlxTimingLayer;

/// [`SqlxTimingLayer`] under the filter it needs. This is the form to install.
pub fn sqlx_timing_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    SqlxTimingLayer.with_filter(SqlxTimingFilter)
}

/// What [`SqlxTimingLayer`] is shown: the `sqlx::query` event, and every span.
///
/// Asking for the event at `DEBUG` is also what makes sqlx emit it — it only
/// fires when some subscriber wants it — so this filter is what turns
/// statement timing on, whatever `RUST_LOG` says.
///
/// Every *span*, though, is not over-reach; it is the whole thing working.
/// The event surfaces on a sqlx worker thread whose span stack holds exactly
/// one entry: the innermost span the calling task was inside, which in this
/// stack is `axum_login`'s `call`, not the request span two levels up.
/// `Context::lookup_current` will not hand back a span its own layer's filter
/// rejects, and its fallback re-walks that same one-entry thread stack rather
/// than the parent chain — so a single unseen span between the request span
/// and the query hides the request span completely, and the only symptom is a
/// `db` metric that quietly never appears. Naming the targets to expect would
/// mean re-learning that the next time a middleware is added.
///
/// Being shown a span costs a target comparison, and changes nothing about
/// the log: the `fmt` layer still applies `RUST_LOG` to what it prints.
struct SqlxTimingFilter;

impl<S> tracing_subscriber::layer::Filter<S> for SqlxTimingFilter {
    /// Whether a callsite is a span, and what target it has, are both fixed at
    /// compile time, so this is exact and the interest cache still applies.
    fn callsite_enabled(
        &self,
        meta: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if Self::wanted(meta) {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn enabled(&self, meta: &tracing::Metadata<'_>, _: &Context<'_, S>) -> bool {
        Self::wanted(meta)
    }

    /// `DEBUG` is what this layer *needs*, not a ceiling on what it accepts:
    /// no span above the process-wide maximum is ever created, so capping the
    /// hint here keeps that maximum where sqlx's own event already puts it
    /// rather than dragging it to `TRACE`. A `RUST_LOG` that asks for `TRACE`
    /// raises the maximum itself, and those spans are then let through too.
    fn max_level_hint(&self) -> Option<tracing_subscriber::filter::LevelFilter> {
        Some(tracing_subscriber::filter::LevelFilter::DEBUG)
    }
}

impl SqlxTimingFilter {
    fn wanted(meta: &tracing::Metadata<'_>) -> bool {
        meta.is_span() || meta.target() == SQLX_TARGET
    }
}

impl<S> Layer<S> for SqlxTimingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    /// Hang the task's metrics on the request span as it opens. This runs
    /// synchronously inside [`server_timing`]'s task-local scope, which is the
    /// whole trick: from here on the metrics are reachable from any thread
    /// that enters the span.
    fn on_new_span(
        &self,
        _attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else { return };
        if span.metadata().target() != REQUEST_TARGET {
            return;
        }
        let _ = CURRENT.try_with(|metrics| {
            span.extensions_mut().insert(metrics.clone());
        });
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != SQLX_TARGET {
            return;
        }
        let mut elapsed = ElapsedSecs(None);
        event.record(&mut elapsed);
        let Some(elapsed) = elapsed.0 else { return };
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };
        for span in scope {
            if let Some(metrics) = span.extensions().get::<Arc<RequestMetrics>>() {
                metrics.add_db(Duration::from_secs_f64(elapsed));
                return;
            }
        }
    }
}

/// Pulls `elapsed_secs` off a `sqlx::query` event — the numeric field sqlx
/// emits next to the human-readable `elapsed`.
struct ElapsedSecs(Option<f64>);

impl tracing::field::Visit for ElapsedSecs {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.0 = Some(value);
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
