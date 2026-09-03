# Step 4 handoff — table-of-contents sidebar

Branch `v2-toc`, worktree `/home/thallada/workspace/the-daily-epub-v2d`, one commit.
Builds on step 2's design system (`handoff-step2.md`) and step 1's legacy fallback
(`handoff-step1.md`). Also folds in one reader-side fix from the step-2 review: the
phone "ears" row.

## Landed

### Data (`src/web/issue.rs`)

- `issue_toc(&IssueView, TocPosition) -> Toc`, one helper feeding all four signed-in
  handlers (`render_full`, `article`, `world`, `behind`) as the template field `toc`.
- `TocPosition` (`FrontPage`, `Article(ArticleId)`, `World`, `Behind`) and `TocKind`
  (`Brief`, `Section`, `Chapter`, `World`, `Behind`, `Colophon`) as the brief specifies.
- Chapters are the picks in `chapters::section_names` × `Lineup::section_picks` order —
  the same order the issue index and the EPUB use — numbered `1..n` across sections with
  the section names interleaved as non-links. World Briefing and Behind the paper follow
  when available (`world_briefing.is_some() || world_html.is_some()`, and `has_behind`,
  both as computed after step 1), then a `#colophon` anchor on the issue page.
- `position` / `total` count navigable chapters only (articles + world + behind), so an
  article page says "Chapter 4 of 11"; the issue page has `position == 0` and reads
  "Front page".
- New small helpers: `issue_href(date)` (replaces four inline `format!`s) and
  `crate::pipeline::short_display_date(date)` → `"Wed, Sep 2"`.

### Markup (`src/web/templates/_toc.html`, included by the four reader templates)

- One `<nav id="toc" data-toc-panel>` for both breakpoints, wrapped with the article in a
  `lg:grid lg:grid-cols-[16rem_minmax(0,1fr)] xl:grid-cols-[18rem_minmax(0,1fr)]` shell
  inside `max-w-7xl`. The reading column keeps `max-w-[68ch]`; at `lg` (1024px viewport)
  it still gets ~680px, comfortably above the ~60ch floor the brief asks for.
- Desktop: `lg:sticky lg:top-6 lg:max-h-[calc(100vh-3rem)] lg:overflow-y-auto`, thin
  scrollbar. Header (issue number + short date linking to the issue, "Chapter N of M",
  2px progress bar) is `lg:sticky lg:top-0` **inside** the scrollable nav, so it stays put
  while the chapter list scrolls under it. Section labels are tracked uppercase sans;
  chapters are numbered Newsreader `text-[0.95rem]` links with the read time in muted
  sans; the current one is bold with a 2px accent bar and `aria-current="page"`. World
  Briefing / Behind the paper / Colophon follow a hairline.
- Below `lg`: a sticky bar (`sticky top-0 z-30` on the wrapper, `lg:static`) with the
  hamburger `<button data-toc-toggle aria-controls="toc" aria-expanded>`, the current
  chapter title truncated, "4 / 11", and the progress bar as its bottom edge. The same
  `<nav>` drops under it as an absolutely positioned panel.
- No inline `<style>`/`<script>` and no `style=""`; every hook step 2 listed is intact.

### Behaviour (`src/web/static/app.js`, `theme.js`)

- The panel toggles by `data-open` on the nav; `@media (width < 64rem) .has-js
  [data-toc-panel]:not([data-open="true"]) { display:none }` in `tailwind.css`. `theme.js`
  (synchronous, in `<head>`) adds `has-js` to `<html>`, so **without JS the panel is simply
  visible** under the bar and **with JS there is no open-then-collapse flash**.
- Closes on link tap, on Escape (focus returns to the toggle), on a tap outside, and when
  crossing to `lg`. Body scroll is never locked.
- `revealCurrent()` scrolls the panel — never the page — the minimum needed to bring the
  current chapter into view, on load and on open. Long issues no longer hide the reader's
  position behind the fold.
- Article pages (`<article data-toc-scroll>`) update every `[data-toc-progress]` with
  `position - 1 + scrolled fraction`, throttled through `requestAnimationFrame` on a
  passive scroll listener.

### Progress bar and CSP

The bar is a real `<progress class="toc-progress">`: a percentage width would need a
`style=""` attribute, which the CSP forbids, and Tailwind cannot generate a class for a
runtime number. It is styled in `@layer components` (2px, `--rule` track, `--accent`
value) and marked `aria-hidden="true"` because the adjacent text already says
"Chapter N of M". Reduced motion: the 150ms transition on the value is dropped both under
`prefers-reduced-motion: reduce` and while JS drives the bar (`[data-live]`), so scrolling
never animates.

### Ears row (step-2 review follow-up)

`layout.html`: the row is `flex-nowrap` with `gap-2 sm:gap-4`; the left cell is
`min-w-0 truncate whitespace-nowrap`; the right group and the theme toggle are
`shrink-0 whitespace-nowrap`; "Sign in" is `whitespace-nowrap` and a long username is
`max-w-32 truncate`. The four issue pages and `issue_public.html` now show
`short_display_date` below `sm:` and the full dateline + issue number above it, so at
390px the whole row is one line for anonymous, reader and admin.

## Tests

- `cargo test` — **478 passed, 0 failed** (442 lib + 9 bin + 2 + 3 + 4 + 7 + 9 + 2
  integration, 0 doc-tests). Run outside the Codex sandbox, so the socket-binding tests
  ran too.
- New: `web::issue::tests::toc_numbers_chapters_across_sections_and_tracks_the_reader`
  (unit — full item shape across two sections, `total == 4`, `position` for front page /
  article / world / behind, the single back-matter divider, `current_label`, short date)
  and `web::issue::tests::the_sidebar_marks_the_current_chapter_on_every_signed_in_issue_page`
  (router — all four pages carry the bar and panel, the right "Chapter N of M", the right
  `aria-current="page"` href and the colophon anchor; the anonymous issue page has none).
- Extended: the public-page test now asserts no `data-toc-toggle` / `id="toc"` leaks; the
  legacy tests assert the sidebar offers World Briefing and Behind the paper, and that it
  omits World when there is no EPUB to recover it from.
- Browser checks (Playwright, `shoot/toc-behaviour.mjs`): 14/14 — collapsed on load,
  `aria-expanded`/`aria-controls`, open on tap, close on Escape with focus return, close on
  outside tap, close + navigate on link tap, `aria-current` after navigation, desktop panel
  always visible with the bar hidden, progress 4 → 5 on scrolling a chapter, no toc on the
  public page, no console errors.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `npm run css`,
  `npm run css:check` — all clean.

## Deviations

- **`TocItem` has a seventh field, `divider: bool`.** The brief's hairline "after the
  chapters" cannot be expressed in askama without knowing which entry starts the
  back-matter group; the helper marks it instead. Everything else matches the brief's
  shapes. `Toc` also carries `short_date` (for the header and the ears row) and gains one
  method, `current_label()`, for the mobile bar.
- **The toggled element is the `<nav id="toc">` itself**, and the issue header lives inside
  it (hidden below `lg`, where the sticky bar already shows the same information). There is
  still exactly one copy of the list, as the brief requires.
- **The progress bar is a `<progress>` element**, not a div with a percentage width — see
  above; a width would need an inline style.
- **Visibility is a CSS rule keyed on `.has-js` + `data-open`**, not the `hidden`
  attribute: `[hidden]` would also hide the desktop sidebar, and a server-rendered `hidden`
  would break the no-JS case.
- **`theme.js` gained one line** (`classList.add("has-js")`). It is the only script that
  runs before paint, and without it the mobile panel flashes open on every load.
- Article top margins were normalised to `mt-10 lg:mt-12` across the four pages so the
  sidebar and the reading column start on the same line.
- `web::public::PublicIssue` gained `short_date` (no private data; the public template
  needed the phone dateline).

## Screenshots

`/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/e963db53-510f-4312-ac87-460291a92781/scratchpad/step4-shots/`
— 137 PNGs. Eight paths (`/issues/2026-09-02`, `…/articles/1`, `…/articles/5` — mid-issue,
`…/world`, `…/behind`, and the legacy `/issues/2026-09-01` with its `/world` and `/behind`)
× {reader, admin} × {light, dark} × {1280, 1024, 390 mobile closed, 390 mobile open}, named
`<path>-<user>-<theme>-<width|mobile>[-open].png`. Plus `-390-ears` crops of the phone ears
row for anonymous, reader and admin, `-1280-mid` (scrolled, progress bar advanced) and
`-1280-tall` (1500px, the whole sidebar including the back-matter group).

The Playwright helper is `shoot/shoot4.mjs` (adds `--width/--height`, `--click`, `--scroll`
and `--tag` to step 2's `shoot.mjs`, and aborts loudly if the sign-in is throttled).

## Notes for whoever runs the dev server next

- `src/web/static/app.css`, `app.js`, `theme.js` and the templates are all compiled into
  the binary (`include_str!` / askama), so **the server must be rebuilt and restarted after
  every CSS or template edit** — a browser reload alone shows the old assets.
- `server.login_attempts` defaults to 10 per 15 minutes per IP. A screenshot sweep signs in
  once per invocation and trips the throttle after ten runs, which silently yields
  screenshots of the sign-in page. The throwaway `./dev/config.toml` was given
  `login_attempts = 2000`; `dev/` is git-ignored and `seed_dev_db` rewrites it.

## Left for later

- Nothing from step 4. On a very long issue the sidebar is taller than the viewport at
  1280×900, so the back-matter group needs a scroll of the sidebar (by design — the brief's
  `max-h`); `revealCurrent` handles the reader's own position.
