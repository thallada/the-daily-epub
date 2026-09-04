# Task 2 — rating widget, issue section headers, ratings-table verdict column

Worktree: `/home/thallada/workspace/the-daily-epub-ui2` (branch `ui-rating`). Dev port 3602.
Files you own: `src/web/templates/_rating_widget.html`, the `.rating*`,
`td .rating`, `.prose-body` and reader-typography parts of `src/web/tailwind.css`,
`src/web/templates/issue_full.html` and `issue_public.html` (section headers),
`src/web/templates/article.html` (rating placement only),
`src/web/templates/dashboard/ratings.html`, and the rating `submit` handler in
`src/web/static/app.js` if the markup change needs it. Read the shared brief first.

## 1. Redesign the rating widget (operator issue 3)

`_rating_widget.html` renders "Was this a good pick? [Loved it][Good][Not for me]
clear" (+ a Note input on dashboard pages). The buttons are 40 px tall,
full-border segmented, solid-ink when active — too heavy next to the serif index
cards on the issue front page and the article footer; they read as a form, not as
part of the paper. Redesign it to be cohesive with the article display:

- Quieter and smaller: sans, `text-xs`/`text-[0.8rem]`, compact height (~30–32 px
  visible; extend the hit area to 44 px on phones with a pseudo-element), hairline
  `rule` borders or a single underlined-text style, `rounded-sm`.
- The active state should use the semantic colour of the verdict (`loved`, `good`,
  `down`) as a tint, like `.badge.loved` etc. (`color-mix(in oklab, var(--loved)
  14%, transparent)` background + coloured text/border), not solid ink — so the
  chosen verdict is legible at a glance and matches the badges used elsewhere.
- The prompt "Was this a good pick?" becomes a small serif-italic or muted-sans
  lead-in that sits on the same line as the buttons at desktop widths and above
  them on phones; "clear" stays a quiet text link, visible only when a verdict is
  active (still in the DOM for the handler; use `[hidden]` or an opacity class).
- Keep the markup contract: `form.rating`, `button[type=submit][name=label][value][data-label]`,
  `.active`, `aria-pressed`, `button.clear`, `label.rating-note > input[name=note]`,
  the three hidden inputs, `widget.show_note`, and the JSON fetch in app.js (it
  toggles `.active`/`aria-pressed`; if you add a per-verdict tint class, derive it
  from `data-label` in CSS — `.rating button.active[data-label="loved"]` — so JS
  needs no change).
- Press feedback: `active:scale-[0.96]` with `transition-property: transform,
  background-color, color, border-color` on the buttons.

Check both placements: the index card on `/` (after "Why it's here") and the
article footer on a chapter page; also `/dashboard/articles/<id>` which shows the
widget with the note field.

## 2. Section headers in the issue (operator issue 4)

On the issue front page ("In This Issue") and the public issue page, each section
name (e.g. *Technology*) is a 0.72 rem uppercase muted label above a hairline —
the same style as tiny eyebrows elsewhere — so readers scroll past it without
noticing the section changed. Make section headers read as headers: serif,
`text-2xl` or so, weight 600, `leading-[1.1]`, `tracking-[-0.01em]`,
`text-wrap:balance`, sitting on a **stronger rule** (`border-t-2 border-rule-strong`
or a double rule like the masthead) with generous space above, and a small
uppercase eyebrow ("Section" or the article count, optional) if it helps
hierarchy. Headlines inside the section stay `text-2xl`/lead `text-3xl`, so the
section header must be distinguishable from them by the rule + spacing + smaller
size rather than by being bigger; consider `text-xl` uppercase-tracked serif
small caps if that reads better — screenshot both and pick one. Apply the same
treatment to "The Brief" and "Colophon" headers on the front page so the page has
one heading system, and to the section names on `issue_public.html`. Leave the
TOC sidebar's section labels alone (task 1 owns that file).

Also check article bodies (`.prose-body h2/h3` on a chapter page): they should be
clearly headings against `1.0625rem` body text; adjust only if they are weak.

## 3. Ratings table verdict column (operator issue 7)

`dashboard/ratings.html`, tab "Current": the first column stacks a verdict badge,
the three rating buttons, a Note input and "clear" — busy and janky. Make that
cell one calm unit:

- Drop the separate badge: the tinted active button (from item 1) *is* the verdict.
  If a row is `cleared` show a muted "cleared" state in the group instead.
- A compact segmented group of the three verdicts on one line (fixed width, e.g.
  `w-[13rem]`), the "clear" link tucked at the end of that line, and the note
  field beneath as a quiet single-line input (`placeholder="Add a note…"`,
  bottom-hairline only, no box) that only shows a border on focus. Everything
  left-aligned on one grid so rows line up; no wrapping inside the cell at the
  table's minimum width.
- Vertical rhythm: the cell content top-aligned with the row's article title.
- Keep the note column of the table as is (it shows the stored note).

## Polish while you are there (reader pages)

- `text-wrap: balance` on headlines (`h1`–`h4` in reader pages, TOC excluded);
  `text-wrap: pretty` on `.prose-body p` and index summaries.
- `.prose-body img`: add the depth outline `outline: 1px solid oklch(0 0 0 / 0.1)`
  in light and `oklch(1 0 0 / 0.1)` in dark (pure black/white only, use the
  existing `dark` variant), `outline-offset: -1px`.
- Buttons `.btn`, `.btn-primary`, `.btn-danger`: `active:scale-[0.96]` with named
  transition properties (transform + colours), keeping the reduced-motion rule.

## Verification

Screenshot `/` (signed in as reader), a chapter page footer, `/dashboard/ratings`
(admin, tab Current) and `/dashboard/articles/<id>`, light + dark, desktop + phone.
Exercise a rating click on the dev server (the JSON handler) to confirm the tint
switches. Run `cargo test --lib web::` and the ratings/issue router tests. Write
`handoff-rating.md`.
