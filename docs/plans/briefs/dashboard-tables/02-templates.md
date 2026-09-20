# Task 02 — template audit: every cell says what it holds

Worktree: `/home/thallada/workspace/the-daily-epub-tables-tpl` (branch `tables-tpl`).
Read `00-shared.md` and `docs/dashboard-tables.md` first. Only files under
`src/web/templates/` change in this task (plus your handoff). The CSS lands in a
sibling worktree; assume the four cell classes behave exactly as the doc's
table says (`cell-wrap` floor 10rem / ceiling 32rem with `overflow-wrap:anywhere`,
`cell-tight` nowrap, `td.num` nowrap right-aligned, unclassed cells wrap at
spaces, `th` always wraps, page-level tables grow past the page column when
their content needs it).

Go through every `<table>` in `src/web/templates/dashboard/*.html`,
`src/web/templates/_candidate_row.html` and `src/web/templates/_signals_table.html`
and apply these rules to each `td`:

1. **`cell-tight` only on timestamps, dates, ids and single tokens ≤ ~20
   characters** (`first_seen`, `when`, `started`, `finished`, `requested`,
   `created`, `last_login`, `saved_at`, `changed_at`, `event_at`, `issue_date`,
   `published`, `run.date`, `run.funnel` "310 → 200 → …", `age → decay`,
   `#id`). **Remove it** from feed names/titles (`article.feed`, `candidate.feed`,
   `miss.feed`, `row.feed_credits`, nearest-article `feed`), reasons
   (`article.reason`, `row.reason`, `candidate.reason`), "admitted by" cells,
   usernames / `requested_by` / `saved_by`, roles, sources, kinds, job names,
   and from any cell that combines a badge with more text (stage + run date).
   A `.badge` is already `whitespace-nowrap` on its own, so a badge-only cell
   needs no class.
2. **`cell-wrap` on every free-form column**: titles, URLs, notes, messages,
   "reason or comment", facet values, badge lists (interests), config diff
   before/after, anything a user typed. Keep it where it already is.
3. **`num` on numbers only** (it already is; just make sure no text column
   uses it because it wanted right alignment — `#id` cells may keep `num`).
4. **Long text that should not stretch a row**: if a column can hold hundreds of
   characters (job `message`, rating-import `message`, `note`), keep
   `cell-wrap` and wrap the text in `<div class="line-clamp-3" title="{{ … }}">`
   **only** if the full text is reachable somewhere else on the page or via the
   `title`; otherwise let it wrap. Never put `truncate`/`line-clamp-*` on a `td`.
5. **`dashboard/feeds.html`**: drop `table-fixed` (fixed layout is ignored once
   the page-level table is `width:auto`). Keep the `w-*` hints on the `th`s
   (they act as minimums under auto layout), keep the `line-clamp-*`/`truncate`
   inner elements — they already sit on block children, which is the pattern —
   and put `cell-wrap` on the Feed and Why cells if it is not there. The
   actions cell keeps its forms; give it `cell-tight` only if the buttons wrap
   badly, otherwise leave it unclassed.
6. **`dashboard/run.html` Feeds card** (`<table class="table-fixed">` inside
   `.card`): this table is `w-full` inside a card, so `table-fixed` still works
   there; leave it, but it is the only allowed `table-fixed`.
7. **Header cells**: remove nothing, but make sure no `th` has `whitespace-nowrap`
   or `cell-tight`; long headers such as
   "considered → eligible → triaged → assessed → shortlisted → selected" may wrap.
8. **Wrappers**: every page-level table (more than a handful of columns:
   runs, candidates, articles, ratings current/events, interests, feeds, users,
   jobs, stats runs/costs, settings history, profile versions, near misses,
   funnel) must sit in a `<div class="scroll-x …">` that is a **direct child**
   of `<section class="dashboard …">` — check that no such wrapper is nested in
   a stray `<div>`; tables inside `.card`/`.cards`/`details` stay where they are.
   Keep `tall` where it is today.

Do not change the columns themselves, their order, the text, or any
`data-*`/`id`/`name`/`action`/`aria-*` attribute. Do not touch reader/public
templates (`issue_*.html`, `article.html`, `world.html`, `behind.html`, `_toc.html`).

Verify: `cargo test --lib web::` (router tests assert on text and some class
names; if one breaks only because a class moved, update the test and say so),
then `cargo fmt`/`clippy`. List in the handoff, per template, which cells lost
`cell-tight`, which gained `cell-wrap`, and any clamp you added.
