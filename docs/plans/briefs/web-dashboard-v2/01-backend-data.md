# Step 1 — backend data: public summaries, legacy issue fallback

Worktree: `/home/thallada/workspace/the-daily-epub-v2a` (branch `v2-backend`).
Read `00-shared.md` first. This step is Rust plus tiny template edits; a design
step runs in parallel in another worktree and will restyle every template, so
keep your template changes minimal and semantic (no new CSS, no new classes
beyond what you need for tests).

## A. Public issue pages show summary and "why" (shared brief item 3)

- `src/web/public.rs`: add `summary: Option<String>` and `why: Option<String>`
  to `PublicEntry`, filled by `PublicIssue::from(&Issue)` the same way the
  signed-in index does (`web::issue::summary_for`-style: `pick.summary`, else
  `editorial.summaries[article_id]`, trimmed, empty → `None`; `why = pick.why`).
- `issue_public.html`: after the byline, render `<p class="summary">…</p>` when
  present and `<p class="why"><em>Why it's here: …</em></p>` when present.
  `feed_entry.html` (the Atom `<content>`): include the summary and the why line
  per entry too, so feed readers see the same public presentation.
- Tests: rewrite `public_issue_carries_no_generated_text` (and any sibling
  asserting summaries/why are absent, including in the feed tests) into
  `public_issue_shows_summaries_and_why_but_no_bodies`: summaries and why lines
  present; The Brief text, every article body sentence, the World Briefing text
  and every comment string still absent. Keep the existing assertions that
  titles, authors, sources and comment links are present.
- Docs: in `docs/plans/2026-09-03-web-dashboard.md` §7.1 and §21 add a dated
  note ("2026-09-03: the operator chose to publish summaries and why lines;
  bodies/Brief/World/comments remain private"). Update `README.md` if it
  describes the public page.

## B. Legacy issues: rebuild colophon, Behind the paper and the World Briefing (items 5–6)

Context: `web::issue::load` (src/web/issue.rs) takes the fallback branch when
`issues.issue_json` is NULL. Production has such issues (everything before
2026-09-03). What *does* exist for them:

- `issues.report_json` (column since `0001_init.sql`) — the run's
  `report::RunReport` (counts, `provider_costs`, `cost_usd`, `voyage_cost_usd`,
  `config_json` from `pipeline::resolved_run_config`, `started_at`/`finished_at`,
  timings). Verify which model names `resolved_run_config` records and read
  them from there; if it lacks a name, fall back to the current `Config`
  (`state.config`) the way `pipeline.rs` builds `Models` (look at how
  `curator.llms.bulk/editor` and `summary_model` are resolved) and say so in a
  `generator_version`-style note only if unavoidable.
- `runs` row(s) for the date (`entries_fetched`, `candidates`, `selected`,
  tokens, `cost_usd`, `status`, `provider_costs_json`, `config_json`) — use the
  latest finished run for the date when `issues.report_json` is NULL.
- `candidate_runs` rows for that run (curation v2) — `curate::telemetry::paper_near_misses(db, run_id, 10)`
  gives the near misses exactly as the pipeline does (`pipeline::behind_the_paper`).
- The EPUB on disk (`issues.epub_path`, else `publish::issue_filename(date, Edition::Standard, "epub")`
  under `config.publish.epub_dir`) contains `OEBPS/world.xhtml` when the issue
  had a World Briefing, and `OEBPS/colophon.xhtml` / `OEBPS/behind.xhtml`.

Implement, in the fallback branch of `load` (or a helper module
`src/web/legacy.rs` called from it):

1. **Colophon**: `Colophon { provider_costs, models, entries_fetched, feeds_seen,
   candidates, cost_usd, generator_version }` from the report (or run row). Where
   a number is genuinely unknown leave 0 but make the template say "n/a" rather
   than "0 from 0 feeds" — add `Option`s to `ColophonView` where needed and
   render "n/a". `generated_at` already comes from the `issues` row.
2. **Behind the paper**: `BehindThePaper` built like `pipeline::behind_the_paper`
   from the report counts + `paper_near_misses` + models + total cost +
   `generation_secs` from `started_at`/`finished_at`. Set `has_behind` in
   `issue_full` to "a report or run exists", not `from_json`, and make
   `GET /issues/{date}/behind` render for legacy issues.
3. **World Briefing**: when `world_briefing` is `None` and the EPUB exists, open
   it with the `zip` crate (already a dependency), read `OEBPS/world.xhtml`, and
   take the inner HTML of `<body>` minus the chapter's own leading `<h1>` (the
   template supplies the heading). Store it on `IssueView` as
   `world_html: Option<String>` (or an enum `WorldSource { Briefing(WorldBriefing), Recovered(String) }`)
   and have the `world` handler render either `world::render_xhtml(&briefing)` or
   the recovered body `|safe` — it is our own sanitized XHTML, note that in the
   §16 list in the plan. Set `has_world` accordingly. Missing/corrupt EPUB → no
   world chapter, never an error (log at debug).
4. Keep the JSON branch untouched. Do not backfill `issue_json` in the database.

Also make the dev seed mirror production: in `examples/seed_dev_db.rs` the
legacy issue (2026-09-01) must end with `issues.issue_json = NULL`, an
`issues.report_json` present, and — so the world fallback can be exercised — a
real EPUB for it written into `dev/epubs/` with the fixture World Briefing
(`epub::build_edition`/`build_all` or whatever the builder's public entry is;
read `src/epub/mod.rs`) and its path stored in `issues.epub_path`. Add a few
`candidate_runs` rows for the legacy run via `db` helpers or SQL so near
misses render (look at `web::dashboard::tests::seed()` for the shape of
`signals_json`).

Tests (router tests over a temp DB; see the existing `seeded_issue(false)`
harness in `src/web/issue.rs`): fallback colophon shows the report's counts and
models and no "0 from 0 feeds"; `/issues/{date}/behind` renders for the legacy
issue with the near misses; `/issues/{date}/world` renders the recovered
chapter from a small EPUB built in the test with `epub::build` on the fixture
issue; `/issues/{date}` links to both chapters; `PublicIssue` never gains a
body/brief/world field (extend the existing negative test).

## Handoff

`docs/plans/briefs/web-dashboard-v2/handoff-step1.md`. Commit once on
`v2-backend`: `Web dashboard v2 step 1: public summaries, legacy issue fallback`.
