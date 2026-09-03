# Step 4 handoff — ratings page and profile page

## Landed

- `src/web/dashboard/ratings.rs` + `dashboard/ratings.html` (`GET /dashboard/ratings`,
  plan §10). Header from `PreferenceState::load(..).summary()` (the same loader
  the pipeline uses) plus the "N rated articles have no embedding" notice; the
  "How ratings enter the algorithm" `<details>` with the live config values
  inlined and links to `/dashboard/settings#curation.feedback`,
  `#curation.ranking` and `#curation.ranking.weights.preliminary`.
  **Current** tab: one row per article from
  `current_ratings_including_cleared(36_500)` with the verdict badge and the
  inline rating widget (`show_note = true`), article → `/dashboard/articles/{id}`
  with feed and issue date, when · source · username, note, age → decay,
  neighbour weight (`value × decay`, or "no embedding"; a "(beyond lookback)"
  marker when older than `rating_lookback_days`), feed credit per direct feed,
  in-prompt / in-rebuild ticks from the rank among non-cleared verdicts, and
  "used last run" from `signals_json.neighbours` of the latest non-dry run
  (count of candidates and how many were selected). Filters: label, source,
  feed, q. **Events** tab: every `rating_events` row, 100 per page,
  `superseded` marked via an `EXISTS` on a later explicit event; filters label,
  source, user, from/to date. Filter values are validated (allow-listed labels,
  `[a-z0-9_]` sources, parsed dates/ids) and only ever bound; the dynamic
  `WHERE` is assembled from fixed clause strings under `sqlx::AssertSqlSafe`.
- `src/web/dashboard/profile.rs` + `dashboard/profile.html` (plan §11).
  `GET /dashboard/profile`: the `profile.md` textarea, the server-rendered
  parsed preview (passthrough body in a `<pre>`, extracted `## Interests`
  lines), history (50 newest `profile_versions` with 200-char previews and a
  Restore form), OPML standing interests grouped by `group_into_themes` with
  path and count, the learned adjustments with the prompt version, build time,
  age and whether `profile::is_stale` says a rebuild is due, the collapsed
  system prompt (`kv.taste_profile`), and the "Rebuild profile now" form
  posting to `/dashboard/jobs/profile-rebuild` (disabled with a note when
  `server.jobs_enabled` is false). `POST /dashboard/profile`: CRLF→LF
  normalization, reject empty or > 64 KB (400), record the previous file in
  `profile_versions` with `saved_by`, write `<path>.tmp` + rename preserving
  the existing mode, flash "Saved; the next run rebuilds the system prompt."
  Identical content is a no-op flash with no version row.
  `POST /dashboard/profile/restore` (`version_id`): records the current file
  as a new version and writes the chosen one back; unknown id → 404.
- `src/curate/profile/mod.rs`: `MAX_RATINGS_IN_REBUILD` and `stored_version`
  are now `pub` (the profile page shows the prompt version and build time).
- `src/web/static/app.css`: a `/* step 4 */` block (tabs, filters, contribution
  table, profile grid, previews).

## Deviations and notes

- Feed credit is shown as `value × decay / n`, not the plan §10 table's
  `value / n`: `signals::feed_rates` credits the **decayed** weight ("decayed as
  in §9.2", curation plan §9.3), and the page shows what the ranker computes.
- "Used last run" scans the run's `candidate_runs` rows once
  (`signals_json LIKE '%"neighbours":[{%'`) and parses each `SignalsJson`,
  instead of one `LIKE '%"article_id":<id>,%'` query per rated article; same
  result, one query.
- The verdict block and rebuild set are count-bounded, so the neighbour weight
  is still shown for ratings older than `rating_lookback_days`, with a
  "(beyond lookback)" marker, since `PreferenceState::load` would not load them.
- `RatedArticle` does not carry the event `source`; the Current tab reads it
  with one small query per row (ratings are sparse — hundreds at most).
- `TasteProfile.verdicts` is not persisted, so the prompt's verdict count is
  derived by counting lines after `## Recent verdicts` in the stored text.
- No new migration, no changes to `db.rs`, `web/mod.rs`, `layout.html` or
  `app.js`.

## Left for later steps

- Step 5 must provide the settings anchors linked from the ratings page:
  `curation.feedback`, `curation.ranking`, `curation.ranking.weights.preliminary`.
- Step 6 implements `POST /dashboard/jobs/profile-rebuild`; the profile page
  already renders the form (with `data-confirm`).
- Step 3's article detail pages are linked as `/dashboard/articles/{id}`.

## Verification

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test` (outside the sandbox, nothing skipped): **390 lib tests passed,
  0 failed** (14 new in `web::dashboard::{ratings,profile}`), plus every
  integration suite green (7, 2, 3, 4, 7, 9, 2 tests).
