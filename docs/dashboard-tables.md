# Dashboard tables

How every data table on the dashboard is laid out, and the rules a new table
must follow so it never grows a horizontal scrollbar on a desktop screen.
Everything here is plain CSS in `src/web/tailwind.css` plus four cell classes;
there is no table library and no JavaScript involved.

## The problem this solves

A table used to be `min-width: max-content` inside a `.scroll-x` wrapper, and
most cells were `white-space: nowrap`. Any table whose natural width exceeded
the page column (80rem, ~1355px at the site's 110% scale) became a horizontal
scroller, even on a 2560px monitor, and the columns that mattered were hidden
behind the scrollbar. Wrapping columns were squeezed to their minimum while the
nowrap ones kept everything.

## The three rules

1. **Tables get the whole viewport, everything else keeps the page column.**
   `.dashboard` is a CSS grid with two named column spans: `content` (at most
   80rem minus the page gutters, exactly what the header uses) and `wide` (the
   viewport minus the same gutters). Every direct child sits in `content`; a
   direct-child `.scroll-x` sits in `wide`. The table inside a wide wrapper is
   `width: auto` (shrink-to-fit) with `min-width` equal to the content column
   and `max-width: 100%`, centred. A table that fits the page column therefore
   looks exactly as before; a table that needs more grows, centred, up to the
   viewport edge; the page itself never scrolls sideways.

2. **Columns wrap before the table scrolls.** The `min-width: max-content` rule
   is gone. A table shrinks by wrapping its text columns down to their floors,
   and only when the floors alone no longer fit (phones, mostly) does the
   `.scroll-x` wrapper scroll. Headers always wrap.

3. **A cell declares what it holds, and nothing else decides its width.**
   There are exactly four cell classes:

   | class        | use it for                                                     | behaviour                                                              |
   | ------------ | -------------------------------------------------------------- | ---------------------------------------------------------------------- |
   | *(none)*     | short prose, names, tokens, badges and badge lists             | wraps at spaces (a `.badge` is nowrap on its own, so lists wrap between badges) |
   | `num`        | numbers, money, counts                                         | right-aligned tabular figures; `td.num` never wraps, `th.num` may      |
   | `cell-tight` | timestamps, dates, ids, a single short token that must not split | `white-space: nowrap`. Nothing longer than ~20 characters              |
   | `cell-wrap`  | titles, URLs, notes, messages, anything free-form              | floor 10rem, ceiling 32rem, `overflow-wrap: anywhere`                  |

   Anything long that must not wrap gets an inner block with a Tailwind clamp
   (`<div class="line-clamp-2" title="…">`) inside a `cell-wrap` cell; the cell
   ceiling bounds the column, the clamp bounds the row height. Do not put
   `truncate`/`line-clamp-*` on a `td` itself (a table cell cannot be a
   `-webkit-box`), and do not use `table-fixed` on a page-level table (fixed
   layout needs a definite width and is silently ignored with `width: auto`).

## Checklist for a new table

- Wrap it in `<div class="scroll-x">` and make that wrapper a **direct child** of `<section class="dashboard">` if the
  table has more than a handful of columns. Tables inside `.card`, `.cards` or
  `details` stay inside their box and just wrap/scroll there.
- Let the page scroll. A table that is the page's feature (paginated lists,
  anything that is the last thing on the page) shows every row at full height;
  a scrolling island inside a scrolling page just fights the wheel. Add `tall`
  (70vh cap, own scrollbar) only when the table is a side feature with real
  content below it, as the profile page's version history is.
- Give every `td` one of the four classes above according to its content.
  When in doubt, leave it unclassed: wrapping is the safe default.
- Never `cell-tight` a name, a title, a reason sentence or a free-text field.
- Check the page at 390, 1280 and 2560px. At 1440 and above the wrapper's
  `scrollWidth` must equal its `clientWidth`; at 390 a wide table is allowed to
  scroll. A table's floor is the sum of its nowrap columns plus 10rem per
  `cell-wrap` column: the 13-column articles table bottoms out near 1270px, so
  it still scrolls a little on a 1280px viewport. Tables inside a `.card`
  cannot grow past the card (20rem minimum), so a card table with more than
  four numeric columns (run page: provider usage) scrolls inside its card at
  1280px; move such a table to page level if that matters.

## Measuring

`docs/plans/briefs/dashboard-tables/measure.mjs` logs in as the dev-seed admin,
visits every dashboard page at several widths and prints, per page, whether the
document overflows and which table wrappers scroll. Run it against a seeded
dev server (`cargo run --example seed_dev_db -- ./dev`) before and after a CSS
change; the "scrolling" count for widths ≥ 1280 should be zero.
