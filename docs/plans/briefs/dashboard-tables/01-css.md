# Task 01 — table framework CSS

Worktree: `/home/thallada/workspace/the-daily-epub-tables-css` (branch `tables-css`).
Read `00-shared.md` and `docs/dashboard-tables.md` first. Only `src/web/tailwind.css`
and the rebuilt `src/web/static/app.css` change in this task (plus your handoff).
The template audit happens in a sibling worktree; do not edit templates.

## 1. `.dashboard` becomes a content grid

Replace `.dashboard { @apply mx-auto my-8 max-w-7xl px-4 pb-6 … sm:px-6; }` with a
grid that keeps the page column pixel-identical to the header
(`mx-auto max-w-7xl px-4 sm:px-6` = content width `80rem - 2 * gutter`, centred):

```css
.dashboard {
  --gutter: 1rem;
  @apply my-8 grid pb-6 font-sans text-base leading-normal;
  grid-template-columns:
    [full-start] var(--gutter)
    [wide-start] 1fr
    [content-start] minmax(0, calc(80rem - 2 * var(--gutter)))
    [content-end] 1fr
    [wide-end] var(--gutter)
    [full-end];
}
@media (width >= 40rem) { .dashboard { --gutter: 1.5rem; } }   /* Tailwind `sm` */
.dashboard > * { grid-column: content; min-width: 0; }
.dashboard > .scroll-x { grid-column: wide; }
```

Keep `.dashboard > * + * { mt-4 }` and every other `.dashboard …` rule as they
are (margins on grid items work). Verify in the templates that nothing relied
on the old padding: `.save-bar` uses `-mx-4 sm:-mx-6` to bleed to the edges;
under the grid it still overflows its `content` area into the gutter columns
by the same amount, so leave it unless you see a problem. The `sm` breakpoint
is `40rem` in Tailwind v4 and `@media` rem is always 16px-based, so the query
above matches `sm:` exactly.

Prototype measured in Chromium and Firefox at 390/1280/2560: `h1` left edge and
width identical to today, no document overflow, table centred and wider than
the column only when its content needs it.

## 2. Tables shrink by wrapping; page-level tables may grow past the column

- Delete `.scroll-x > table { min-width:max-content; }`.
- Add, for the wide wrapper only:

```css
.dashboard > .scroll-x > table {
  width: auto;                                   /* shrink-to-fit */
  min-width: min(100%, calc(80rem - 2 * var(--gutter)));
  max-width: 100%;
  margin-inline: auto;
}
```

  A table narrower than the page column fills the column as today; a wider one
  grows, centred, to the viewport edge; the wrapper scrolls only past that.
  Tables that are not direct children (cards, disclosures, signal tables inside
  cells) keep `w-full` from the base rule and simply wrap, then scroll.

## 3. Cell vocabulary (exactly four classes, see the doc's table)

- `.num` → `@apply text-right tabular-nums;` and `td.num { @apply whitespace-nowrap; }`.
  Header cells with `.num` may wrap ("cache write", "matched articles").
- `.cell-tight` unchanged (`whitespace-nowrap`).
- `.cell-wrap` → `@apply min-w-[10rem] max-w-[32rem] whitespace-normal; overflow-wrap:anywhere;`
  (ceiling raised from 24rem so titles get a single line when the screen has
  room; `max-width` on a cell is honoured by Chromium and Firefox for the
  column's max-content contribution — measured).
- `th` keeps wrapping (no nowrap); keep the sticky `thead`.
- `td > pre.preview { max-w-md }` and `.rating-cell` rules stay.

## 4. Small things to keep honest

- `.prose-body table` (reader pages) keeps its own rules; do not touch.
- Put a short comment above the table rules pointing at `docs/dashboard-tables.md`.
- `npm run css`, then `cargo build` (the binary embeds app.css) and
  `cargo test --lib web::`. Router tests assert on markup, not on these rules,
  so nothing should break; if something does, say what and why in the handoff.
- If you can run the dev server + `measure.mjs`, do it and paste the ≥1280 lines
  for `/dashboard/articles`, `/dashboard/runs` and `/dashboard/runs/1` into the
  handoff. With unchanged templates some tables will still scroll at 1280 —
  that is expected until the template audit lands — but at 2560 the articles
  and candidates tables must fit without scrolling.
