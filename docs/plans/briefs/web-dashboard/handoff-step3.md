# Step 3 handoff — dashboard reads: overview, runs, articles

## Landed

- `src/web/dashboard/mod.rs`: the overview (§9.1) — last-run card built from
  `runs.report_json` via `RunReport::info_block()` (legacy-counter fallback
  when the report is missing or unparseable), budget-today `<meter>` per
  referenced provider plus voyage from `db::provider_spend_for_utc_day(now)`,
  ratings this week by label, "Unrated picks" (last three issues' picks with
  no explicit event, each with the step 2 rating widget posting back to
  `/dashboard`), a jobs summary read straight from the `jobs` table (active
  rows plus the last five finished; the pages themselves are step 6), and the
  `! ` lines of `Config::check_report`. Sparklines are a marked HTML comment
  for step 6. The module also carries the helpers the read pages share:
  `Pager`, `SignalsView` (the parsed `signals_json` behind
  `_signals_table.html`), `allow_listed`, `like_pattern`, `Bind`/`bind_all`/
  `dynamic_query`, formatting helpers, and the `STAGES`/`REASONS`/
  `RETRIEVERS` allow-lists.
- `src/web/dashboard/runs.rs` (§9.2): `/dashboard/runs` with `?status=` and
  pagination (50), and `/dashboard/runs/{id}` with header (issue link when the
  issue exists, previous/next run by id), the funnel from `candidate_runs`
  grouped by stage and reason, admission mix, preference state, timings,
  provider usage, warnings (`#warnings` anchor used by the overview and the
  list), top-20 feeds, the config diff (`flatten_json` + `config_diff`)
  against the previous non-dry run that recorded a config, ten near misses via
  `telemetry::near_misses`, and the candidates table (100 per page) with the
  `stage`/`reason`/`admitted_by`/`q`/`flag` filters and `utility` (default)/
  `rank`/`triage`/`quality`/`fit`/`title` sorts. Each row's title is a
  `<details>` that expands `_signals_table.html`.
- `src/web/dashboard/articles.rs` (§9.3): `/dashboard/articles` (50 per page)
  joining the best entry, the latest `candidate_runs` row through the
  `MAX(run_id)` subquery (the index `idx_candidate_runs_article_run` is in
  migration 0004 — confirmed), both assessments, the current explicit rating
  and the latest publication; every listed filter and sort;
  `/dashboard/articles/{id}` with the six blocks in the plan's order,
  `provider_rejected` rows called out as such, run history rows expanding
  their signals, neighbours/interests from the latest row, embedding metadata
  only (never the vector), all rating events with source/user/note/value, the
  rating widget with `show_note = true`, and "Explain (text)" wrapping
  `telemetry::render_explain` verbatim in `<pre>`.
- Templates: `dashboard/overview.html`, `runs.html`, `run.html`,
  `articles.html`, `article.html`, partials `_signals_table.html`,
  `_candidate_row.html` and `dashboard/_pager.html`. Every table sits in
  `.scroll-x`; badges per stage/reason/label/status; tables carry
  `data-filter`.
- `app.css` / `app.js`: one appended block each (`/* step 3 … */`): cards,
  filters, funnel bar, pager, signals details, diff colours, and the
  filter-as-you-type behaviour for `table[data-filter]` (inserts a search
  input before the table's `.scroll-x` wrapper, hides non-matching rows on
  the current page only).

## Deviations and notes

- **sqlx 0.9 dynamic SQL.** `sqlx::query` only accepts `&'static str` or
  `AssertSqlSafe`, so the list queries go through `dashboard::dynamic_query`,
  which wraps the assembled string in `AssertSqlSafe`. The audit holds
  because the string is built only from constants and allow-listed fragments
  (`CANDIDATE_SORTS`/`ARTICLE_SORTS` map names to fixed `ORDER BY` text);
  every user value is bound through `bind_all`. An unknown sort or filter
  value falls back to the default / is dropped, never an error.
- **Funnel semantics.** `stage` records where a row stopped, so the bars
  show the cumulative "reached" count (rows at this stage or a later one, so
  the first bar is everything considered) with "stopped here" and the reason
  breakdown beside it. Widths are `<svg width="N%">` children of `.funnel`
  because the CSP (`style-src 'self'`) forbids inline `style` attributes;
  the budget card uses `<meter>` for the same reason.
- **Prev/next run** links go to the neighbouring run ids overall (the
  header already links the date's issue); a rerun of the same date is
  therefore the immediate neighbour.
- **Extract method** is not shown on the article page: `db::article_from_row`
  hard-codes `ExtractMethod::Miniflux` for every loaded article, so the value
  would be meaningless. `excerpt_only` is shown instead. Source `kind`s render
  via their `Debug` names (`Feed`, `Scour`, …).
- **`admitted_by` filter** is allow-listed against the six retriever names
  and still applied as the plan's prefix `LIKE` on
  `json_extract(admitted_by, '$[0]')`.
- The `_pagination.html` partial from step 1 (text only) was left alone; the
  dashboard uses its own `dashboard/_pager.html` with prev/next links that
  preserve the other query parameters.
- Only additive edits outside my files: two appended blocks in `app.css` and
  `app.js`. No changes to `web/mod.rs`, `db.rs`, `rate.rs` or `telemetry.rs`
  (a private `first_retriever` in telemetry was reimplemented as
  `admitted_by_parts` rather than made `pub`).

## Left for later steps

- Step 6: overview sparklines (placeholder comment in `overview.html`), and
  the `/dashboard/jobs` pages the overview's job links point at.
- Step 4: `/dashboard/ratings`, linked from the overview's ratings card.
- The overview's ratings-this-week card counts all explicit events in the
  last seven days by label (a `cleared` is listed but not counted in the
  total).

## Verification (outside the sandbox)

- `cargo fmt`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test web::dashboard`: **11 passed, 0 failed**.
- `cargo test` (full suite): **387 lib tests passed, 0 failed**, plus every
  integration test binary green (7, 2, 3, 4, 7, 9, 2), nothing skipped.
