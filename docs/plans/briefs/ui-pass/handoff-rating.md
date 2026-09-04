# Task 2 handoff — rating widget, issue section headers, ratings-table verdict column

Branch `ui-rating` in `/home/thallada/workspace/the-daily-epub-ui2`, dev port 3602.
A previous agent (Codex) landed a partial pass as `0cdc302` before hitting a quota
limit and never saw the rendered result; `main` was merged in afterwards
(`bcbe158`). This pass kept the parts of that work that held up on screen, fixed
the parts that did not, and finished the remaining items.

## What changed

### `src/web/tailwind.css`

- **`.reader-section-heading`** — replaced Codex's sentence-case `text-xl` serif
  heading with a serif **uppercase, letter-spaced section mark** on a double rule:
  `border-t-[3px] border-double border-rule-strong pt-3 font-serif text-[1.05rem]
  font-semibold uppercase leading-[1.2] tracking-[0.15em]`. `text-wrap: balance`
  stays. (Rationale under *Design decisions*.)
- **Rating buttons** — inactive labels are now `text-ink-2` (were `text-ink`) and
  go to `text-ink` on hover, so the group recedes next to serif body copy.
- **Rating button hit area** — the `::after` overlay went from `-inset-y-1` /
  `-inset-y-1.5` (phone) to `-inset-y-1.5` / `-inset-y-2`. The pseudo is
  positioned against the *padding* box, so the old values gave 37 px desktop /
  41 px phone; the new values measure 41 px and 45 px (verified with
  `elementFromPoint`), clearing the ≥ 40 px / ≥ 44 px rules in the shared brief.
- **Rating focus ring** — `.rating button[data-label]:focus-visible { z-20
  ring-offset-0 }`. In a segmented group the inherited `ring-offset-2` painted a
  detached red box over the neighbouring buttons; hugging the segment reads as
  one control.
- **`.rating-note input`** — now a quiet bottom-hairline field everywhere
  (`rounded-none border-x-0 border-t-0 border-b border-rule bg-transparent px-0
  py-1 shadow-none focus:border-ink`) instead of a boxed input on reader/dashboard
  pages and a *transparent* bottom border in tables.
- **Table/list overrides** — `td .rating` / `.picks .rating` lost the
  `max-w-[15rem]` cap (the cell now sets its own width) and gained `gap-y-1`;
  table buttons use `px-1.5` so the group fits; `.picks .rating` gets `mt-2` so
  the widget is not welded to the pick title.
- **New `.rating-cell` block** — the ratings-table verdict cell is a one-column
  grid, `w-[15rem]`, `gap-y-1.5`: row 1 is the segmented group with `clear`
  pushed to the right edge (`ml-auto`), row 2 is the note field spanning the same
  width. `min-h-0` on `.rating-actions` inside the cell top-aligns the group with
  the row's article title (measured: 1 px apart).

### `src/web/dashboard/ratings.rs`

- Removed the now-dead `CurrentRow::verdict` field and its assignment (Codex
  dropped `{{ row.verdict }}` from the template but left the field, which fails
  `clippy -D warnings`). `badge` stays — it is still read by the "no embedding" /
  "—" branch in the template.

### `src/web/templates/world.html`, `src/web/templates/behind.html`

- Added `reader-page` to the `<article>` so the polish item's
  `text-wrap: balance` on reader headlines covers the World Briefing and Behind
  the paper chapters too. (Deviation — see below.)

### Kept from Codex's partial commit (verified on screen, unchanged)

`_rating_widget.html` (`.rating-actions` wrapper, `clear`/`cleared` label spans,
`placeholder="Add a note…"`), `article.html` / `issue_full.html` /
`issue_public.html` (`reader-page`, `reader-section-heading`, `index-summary`,
`mt-14` section spacing, colophon and The Brief on the same heading system),
`dashboard/ratings.html` (badge dropped from the verdict cell, `rating-cell`
class), the per-verdict tints, `.prose-body img` outline, `.prose-body p` /
`.index-summary` `text-wrap: pretty`, and `active:scale-[0.96]` with named
transition properties on `.btn` / `.btn-primary` / `.btn-danger`.

### `src/web/static/app.css`

Rebuilt from `tailwind.css` (Tailwind 4.3.3).

## Design decisions

- **Section headers: uppercase mark, not a bigger headline.** The brief asked for
  both options to be screenshotted. A sentence-case serif `text-2xl` header
  (`after-rating/sec/C.png`) is the same size as the article headlines directly
  beneath it and reads as another story, which is the exact failure being fixed.
  The uppercase, letter-spaced serif mark on a double rule
  (`after-rating/sec/B.png`, `B2-*.png`) is unmistakably a section marker at a
  glance and never competes with a headline — the newspaper convention the rest
  of the design language is borrowing. It is also visibly *not* the old 0.72 rem
  sans eyebrow: serif, ink-coloured, ~50 % larger, and sitting on the same double
  rule as the masthead. No extra eyebrow was added; the header is already
  uppercase, so a second uppercase line above it would be noise.
- **`clear` stays `invisible`, not `[hidden]`.** It keeps its layout box, so
  choosing a verdict does not shift the group sideways. `.rating:has(button
  .active) button.clear` reveals it; the cleared state swaps the label to a muted
  "cleared" with no underline, which doubles as the brief's "muted cleared state
  in the group" for the ratings table.
- **Verdict cell is `w-[15rem]`, not `w-[13rem]`.** 13 rem is 208 px; the three
  labels plus `clear` measure ~219 px at `text-xs`/`px-1.5`, and the wider
  "cleared" label needs ~237 px. 15 rem (240 px) fits both states with the
  `clear`/`cleared` link flush right in each, so rows stay aligned whichever state
  they are in. Shrinking the type to ~0.7 rem would have hit 13 rem but is too
  small for a dense table that is otherwise 14 px.
- **Note field shows a faint rule at rest.** The brief said "bottom-hairline only,
  no box" *and* "only shows a border on focus"; Codex read that as a transparent
  border, which left "Add a note…" floating with no affordance and no shared
  baseline across rows. A `--rule` hairline that goes to `--ink` on focus keeps
  the cell calm, gives the unit a base line, and still reads as quiet.
- **`.prose-body h2`/`h3` left alone.** Checked against 1.0625 rem body copy
  (`prose-heading-light.png`): `text-2xl` semibold over a hairline is already a
  clear heading. The brief said to adjust only if weak.

## Deviations

- `world.html` and `behind.html` are not in this task's file list, but the polish
  item asks for `text-wrap: balance` on reader-page headlines and those two are
  reader pages. The change is one class per file and touches nothing else.
- Verdict cell width 15 rem rather than the brief's example 13 rem (reason above).
- Section header is `text-[1.05rem]` uppercase rather than `text-2xl`/`text-xl`
  sentence case (reason above).

## Verification

Dev server on `127.0.0.1:3602` against this worktree's own copy of the dev DB
(`dev/config.toml` was repointed from the main checkout's `dev/` to
`the-daily-epub-ui2/dev/` so rating clicks could not touch the shared database;
`dev/` is git-ignored).

- `cargo fmt -- --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test --lib web::` — 84 passed, 0 failed.
- `cargo test` — 445 lib tests + all integration tests passed, 0 failed. (One
  earlier run had a single flaky failure in
  `config::tests::registry_validation_rejects_bad_roles_kinds_and_efforts`; it
  passes in isolation and on re-run, and is unrelated to this change — the host
  was building for three agents at once.)
- `npm run css:check` — no diff (Tailwind 4.3.3 from the adjacent checkout's
  `node_modules/.bin` via `PATH`; only read, never modified).
- **Live JS handler** (`interact.mjs`): clicked loved → good → not-for-me → clear
  → loved on `/issues/2026-09-02/articles/4`. `.active` and `aria-pressed` follow
  every click, the tint switches, **0 page navigations**, console clean (no errors
  or warnings). Also typed into the note field and confirmed it round-trips into
  the ratings table's Note column.
- **Hit areas** measured with `elementFromPoint`: 41 px desktop, 45 px phone.
- **Top alignment** measured: verdict group top is 1 px from the article title top.

### Screenshots

Under `/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/553c7d77-d8f4-44f8-a379-73cedea23814/scratchpad/after-rating/`:

- `sec/B.png`, `sec/C.png`, `sec/B2-brief.png`, `sec/B2-colophon.png` — the
  section-header variant comparison behind the decision above.
- `wip/*` — Codex's partial state before this pass (the overflowing verdict cell
  is `wip/ratings-cell-light.png`).
- `final/` — the shipped result:
  - `/` index card + section: `index-card-light`, `index-card-mobile`,
    `index-card-mobile-dark`, `index-section-light`, `index-section-dark`,
    `index-section-mobile`
  - front-page heading system: `brief-light`, `brief-dark`, `colophon-light`,
    `colophon-dark`
  - anonymous issue page: `public-issue-light`, `public-issue-dark`
  - chapter footer: `article-footer-light`, `article-footer-mobile`
  - ratings table: `ratings-light`, `ratings-dark`, `ratings-mobile`,
    `ratings-mobile-dark`, `ratings-cleared-light`, `ratings-cleared-dark`
  - dashboard article widget: `dash-article-light`, `dash-article-dark`,
    `dash-article-mobile`
  - states: `rating-focus`, `rating-hover`, `overview-picks`
  - polish checks: `prose-heading-light`, `prose-img-light`, `prose-img-dark`

## Left open

- Nothing from this brief. The dev server is left running on 3602 against
  `the-daily-epub-ui2/dev/daily-epub.db`, which now holds 5 current verdicts
  (loved / good / not-for-me / good / loved, one with a note) plus the extra
  rating events those clicks appended — `rating_events` is append-only, so the
  history is longer than the seeded baseline.
