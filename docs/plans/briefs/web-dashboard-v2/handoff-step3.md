# Step 3 handoff — dashboard restyle

Branch `v2-dashboard`, worktree `/home/thallada/workspace/the-daily-epub-v2c`, one commit
on top of the merged `v2-frontend` work (step 1 + step 2).

## Landed

Every template under `src/web/templates/dashboard/` plus the shared partials
`_candidate_row.html`, `_signals_table.html` and `_pagination.html` is restyled with
Tailwind utilities and the component layer step 2 built. `src/web/static/app.css` is
rebuilt and committed; `npm run css:check` is clean.

### New/changed pieces in `src/web/tailwind.css`

`@layer base` gained: a disabled state for every control, `summary { cursor:pointer }`,
and a styled `<meter>` (webkit + moz pseudo-elements, `--rule` track, `--good` value,
`--down` when `.over`) for the overview budget bars.

`@layer components` gained the dashboard vocabulary the templates now use:

| class | what it is |
| --- | --- |
| `.page-head`, `.page-desc`, `.page-actions`, `.page-eyebrow` | the page header on all 13 pages: eyebrow/breadcrumb, `h1`, one-line description, right-aligned actions, hairline under it |
| `.tiles`, `.tile`, `.tile-label`, `.tile-num`, `.tile-delta` | stat tiles (overview, stats, run detail) |
| `.cell-wrap`, `.cell-tight` | long text cells (`message`, `editor_why`, notes, reasons) wrap with `overflow-wrap:anywhere` and a max width; dates/badges stay on one line |
| `.scroll-x.tall` | opt-in `max-h-[70vh] overflow-y-auto`, which is what actually makes the base layer's `thead { sticky top-0 bg-paper }` do something. Applied to the long tables (runs, candidates, articles, ratings, jobs history, users, settings history, stats) and deliberately not to the small tables inside cards |
| `.filters` / `.filter-actions` / `.form-inline` | one inline filter form per page: rule-bounded row, tiny uppercase labels above each control, buttons grouped at the end |
| `.pager`, `.tabs`, `.pagination` | pagination row and the ratings tab strip (accent underline on `.active`) |
| `.setting`, `.setting-label`, `.setting-input`, `.help`, `.default`, `.secret`, `button.reset`, `.save-bar` | settings: two-column rows collapsing to one on phones, monospace keys, disabled env-locked fields with a `locked` badge and a lock note, sticky save bar with a soft top shadow |
| `.disclosure` (`details.disclosure` + `.disclosure-body`) | the framed `<details>` blocks (ratings "How ratings enter the algorithm", stats "As text", article "Explain") |
| `.sparklines`, `.spark-figure`, `.spark-axis`, `.spark-legend` | sparkline cards; the SVG classes (`.spark`, `.s0`…`.s5`, `.line`) are untouched |
| `.funnel`, `.funnel-cell` | the funnel bar gets a `bg-paper-2` track; `class="funnel"` is unchanged because a test asserts it verbatim |
| `td .rating` / `.picks .rating` overrides | the rating widget compacted for table cells and the overview picks list: prompt hidden, `min-h-8` buttons, small note field, capped width |
| `tr.superseded` | muted row (the class stays exactly `superseded`; a test counts it) |
| `.badge` default | bare `<span class="badge">` now has a neutral tint, plus `.badge.restart` / `.badge.warn` |

`.dashboard` gained a vertical rhythm (`> * + * { mt-4 }`), section headings as tracked
uppercase labels on a rule (`.dashboard > h2`), muted underlined table links, and
`code` chips on `bg-paper-2`.

### Per page

- **overview** — five stat tiles (last run, warnings, verdicts this week, unrated picks,
  active jobs) above the existing cards; budget rows are now provider + spend + a meter bar;
  jobs/picks lists are hairline-separated with ink links.
- **runs** — page header with Stats/Jobs actions, inline status filter with a Reset that
  only appears when a filter is set, numeric columns right-aligned, empty-state row.
- **run** — breadcrumb + status badge header, prev/issue/next as buttons in the header,
  four tiles (date, duration, cost, warnings), funnel with a track, cards for the report,
  empty-state row for candidates.
- **articles / article** — filter form, wrapped title cells; the article page splits its
  facts into "Article" and "Provenance" cards, gives the rating widget its own
  "Your verdict" section and keeps everything else as tables.
- **ratings** — description as the page header, tab strip, per-tab filter form, the
  in-cell rating widget, muted superseded rows.
- **profile** — editor/preview split (`lg:grid-cols-2`, stacked on phones), monospace
  textarea, previews as capped `pre.preview` blocks, history table, `.kv` theme list.
- **stats** — window buttons in the header, `summary` rendered as six stat tiles,
  sparkline cards, cards, two filterable tables, text output in a disclosure.
- **jobs / job** — job cards now carry a status badge and a "last started" line linking
  to that job (see the one backend change below); the job page is breadcrumb + two cards
  + a scrolling monospace journal.
- **settings / settings history / users** — as described in the table above.

### Backend changes (three, all small)

1. `JobCard` gained `last_status` / `last_requested` / `last_id`, filled by a new
   `attach_last_runs` from the job rows the page already loads (matching `name` or
   `name-<date>` for the dated `generate` card). No extra query. Unit-tested.
2. `stats::stats` passed `"dashboard"` as its nav key, so the admin nav highlighted
   *Overview* on the Stats page; it now passes `"stats"`.
3. `settings.html` dropped the `! ` prefix in front of the "config.toml on disk does not
   load" banner (the `.error` bar already says it); the assertion in
   `settings_pages_render_save_and_show_hand_edits` was updated with it. That is the only
   test touched.

## Deviations

- **`class="inline"` was a latent bug**, the same shape as step 2's `.block` collision:
  Tailwind emits an `.inline { display:inline }` utility, which beats `form.inline`'s
  `@apply flex` in the components layer, so those forms were laid out inline. The class
  is renamed to `form-inline` in `jobs.html`, `profile.html` and `settings.html` and in
  `tailwind.css`. Nothing asserted on it.
- **Sticky table heads need a scroll box.** `thead { sticky top-0 }` from step 2 is inert
  inside a plain `overflow-x:auto` container, so the long tables opt into
  `.scroll-x.tall` (`max-h-[70vh]`). Small tables inside cards keep the plain `.scroll-x`.
- The brief asks for "last run" on the job cards; the data was not in `JobCard`, so it was
  added (item 1 above) rather than skipped.
- `_rating_widget.html` itself is untouched — two tests assert its exact attribute order
  (`value="loved" data-label="loved" class="active"`), so the table-cell treatment is done
  purely with `td .rating …` rules.
- `.dashboard > h2` renders section headings as small tracked uppercase labels on a rule
  rather than as large sans headings; `.card h2`/`h3` stay normal-weight `text-base`.

## Hooks and asserted classes (all preserved)

`form.rating`, `button[data-label]`, `.active` + `aria-pressed`, `form[data-confirm]`,
`details[id]` (`ratings-how`, `profile-prompt`, plus new `stats-text` and `article-explain`),
`table[data-filter]`, `.table-filter`, `button[data-reset]`/`data-default`, `[data-refresh]`,
`.scroll-x`, `.spark`/`.s0`–`.s5`/`.line`, `class="funnel"`, `class="superseded"`,
`badge <state>`, `<h1>Users</h1>`, every form `action`/`name`/`id` and every `aria-*`.

## Verification

- `npm run css` + `npm run css:check` — clean.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test` — **477 passed, 0 failed** (441 lib + 9 + 2 + 3 + 4 + 7 + 9 + 2 integration,
  one binary with 0 tests). Run outside the Codex sandbox, so the socket-binding tests ran.
- Dev server on `127.0.0.1:3601` (port 3599/3602 belong to other agents, 3499 to
  production). Every dashboard route returned 200, with **no console errors and no asset
  404s** in the server log.

### Screenshots

`/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/e963db53-510f-4312-ac87-460291a92781/scratchpad/step3-shots/`
— 58 PNGs, all signed in as `admin`: `/dashboard`, `/dashboard/runs`, `/dashboard/runs/2`,
`/dashboard/articles`, `/dashboard/articles/1`, `/dashboard/ratings`,
`/dashboard/ratings?tab=events`, `/dashboard/profile`, `/dashboard/stats`,
`/dashboard/jobs`, `/dashboard/jobs/1`, `/dashboard/settings`,
`/dashboard/settings/history`, `/dashboard/users` × {light, dark} × {1280px desktop,
390px mobile}, full page, plus viewport shots of the settings save bar
(`*-top.png`, desktop light/dark and mobile).

The throwaway `./dev` database was extended by hand for the shoot (four `jobs` rows, eight
`rating_events`, four `config_changes`) so the jobs, ratings and settings-history pages had
content; `dev/` is git-ignored and `examples/seed_dev_db.rs` was **not** changed. A future
design pass may want those rows in the seeder.

## Left for later

- Step 4 adds the collapsing table-of-contents sidebar to the signed-in issue pages.
- `examples/seed_dev_db.rs` still seeds no jobs, ratings or config changes.
