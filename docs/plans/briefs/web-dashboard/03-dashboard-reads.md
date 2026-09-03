# Step 3 — Dashboard reads: overview, runs, articles

Read `00-shared.md`, the plan (§3, §4, §9 in full, §16, §17 "Dashboard
queries"), the curation v2 plan sections it cites
(`docs/plans/2026-09-02-personalized-curation-v2.md` §7 tables, §9–§13
signals/admission/utility/editor, §15 explain/stats) and the handoffs for
steps 1–2. This is plan §19 item 3. Study `src/curate/telemetry.rs`
(`ExplainRow`, `resolve_run`, `explain_row`, `near_misses`, `render_explain`,
`SignalsJson`) — the dashboard renders the same data as HTML and must reuse
those loaders/types where they fit rather than re-deriving them.

## Deliverables

1. `src/web/dashboard/mod.rs` — the overview (§9.1): last-run card from
   `runs.report_json` (counters fallback), budget-today card per provider
   (`config.referenced_providers()` + voyage, `db::provider_spend_for_utc_day`),
   ratings this week by label, "Unrated picks" (last three issues' picks with
   no explicit event, with the inline rating widget from step 2), jobs summary
   (the `jobs` table; the pages come in step 6), config-on-disk `! ` lines from
   `Config::check_report`. Sparklines are step 6 — leave a clearly marked
   placeholder section or omit.
2. `src/web/dashboard/runs.rs` (§9.2): the list with `?status=` filter and
   pagination; the detail with header + prev/next run, the funnel (from
   `candidate_runs` grouped by stage/reason, in pipeline order, bars via the
   `.funnel` CSS), admission mix, preference state, timings, provider usage,
   warnings, per-feed top 20, the **config diff** against the previous non-dry
   run (flatten both `config_json` documents to dotted keys), near misses via
   `telemetry::near_misses`, and the candidates table with the allow-listed
   filters (`stage`, `reason`, `admitted_by` via
   `json_extract(admitted_by,'$[0]')`, `q`, `flag`) and sorts (utility default,
   rank, triage, quality, fit, title), 100 per page, a `<details>` per row with
   `_signals_table.html` built from `signals_json`.
3. `src/web/dashboard/articles.rs` (§9.3): the list with the latest
   `candidate_runs` row join (the index `idx_candidate_runs_article_run` exists
   from migration 0004 — confirm), all listed filters and sorts allow-listed,
   50 per page; the detail with the six blocks in the plan's order (article
   facts, assessments incl. `provider_rejected`, run history with expandable
   signals, neighbours/interests, embedding metadata never the vector, rating
   events). The "Explain (text)" `<details>` wraps `telemetry::render_explain`
   output in `<pre>`. The rating widget here carries the note field.
4. Templates: `dashboard/overview.html`, `dashboard/runs.html`,
   `dashboard/run.html`, `dashboard/articles.html`, `dashboard/article.html`,
   partials `_signals_table.html`, `_candidate_row.html`. Every table inside
   `.scroll-x`; badges per stage/reason/label; sticky thead; the `data-filter`
   client-side filter from §4.4 in `app.js`.
5. Sort/filter parameters are validated against allow-lists and never
   interpolated into SQL; an unknown sort falls back to the default, never
   errors.
6. Tests (§17 "Dashboard queries"): funnel counts against a seeded
   `candidate_runs` set; candidate filters/sorts allow-listed; config diff finds
   changed dotted keys and ignores unchanged; articles list filters; article
   detail shows assessments, run history and rating events. Router-level tests
   for the admin guard on each new route.
