# Step 3 — dashboard restyle on the Tailwind system

Worktree and branch: given in your launch prompt. Read `00-shared.md`
(design language, "Dashboard" and "Controls" paragraphs) and
`handoff-step2.md` (what the design system provides: tokens, `.btn`, `.badge`,
`.kv`, `.scroll-x`, `.prose-body`, table and form defaults in
`src/web/tailwind.css`). Step 2 deliberately left `src/web/templates/dashboard/*`
unstyled; you restyle every one of them with Tailwind utilities plus those
components, then rebuild `src/web/static/app.css` (`npm run css`) and commit it.

Templates: `overview`, `runs`, `run`, `articles`, `article`, `ratings`,
`profile`, `stats`, `settings`, `settings_history`, `jobs`, `job`, `users`,
`_pager`, `_sparkline`, and the shared partials `_candidate_row.html`,
`_signals_table.html`, `_pagination.html` (the reader-side `_rating_widget.html`
is styled already; the dashboard variant with the note field must look right
inside tables and on the article page).

Rules of the road:

- Keep every handler field, form `action`/`name`/`id`, `data-*` hook and the
  SVG classes the sparkline/funnel code emits (`.spark`, `.s0`…`.s5`, `.line`,
  `.funnel`). Read `src/web/static/app.js` before touching anything it queries.
- Old CSS class names that tests assert on (grep `tests/` and the `#[cfg(test)]`
  modules under `src/web/dashboard/` for `class=` and for names like `badge`,
  `num`, `muted`, `card`, `superseded`) may stay as harmless extra classes or be
  updated together with the test — never leave a test asserting a class the
  template no longer emits.
- Page header pattern on every page: title, one-line description, actions
  (buttons/links) right-aligned, then filters, then content. Stat tiles on the
  overview and stats pages. Tables per the shared brief; long text cells
  (`message`, `editor_why`) wrap with `overflow-wrap:anywhere`; numeric columns
  right-aligned `tabular-nums`; sticky `thead` on `bg-paper`.
- Settings: grouped cards with monospace section keys, a two-column setting row
  (label + help left, control right) collapsing on phones, locked (env-overridden)
  fields visibly disabled with a small lock note, the sticky save bar, danger
  actions in `text-down`. Profile: editor/preview split that stacks on phones.
  Jobs: cards with a status badge, last run, and the start form; the job page's
  journal in a scrolling monospace block. Users: a simple table.
- Both themes, desktop and mobile (390px), every page: screenshot as `admin`
  with the seeded dev server and **look** before finishing.

Finish: `npm run css` + `npm run css:check`, `cargo fmt`, clippy `-D warnings`,
`cargo test`; write `handoff-step3.md`; commit once:
`Web dashboard v2 step 3: dashboard restyle`.
