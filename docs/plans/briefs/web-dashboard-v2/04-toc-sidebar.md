# Step 4 — table-of-contents sidebar for signed-in issue pages

Worktree and branch: given in your launch prompt. Read `00-shared.md`,
`handoff-step1.md` (legacy world/behind availability) and `handoff-step2.md`
(design system). Item 4 of the shared brief: on every signed-in issue page —
`/issues/{date}`, `/issues/{date}/articles/{id}`, `/issues/{date}/world`,
`/issues/{date}/behind` — a sidebar lists the issue's chapters, marks where the
reader is, and lets them jump anywhere. Public pages get nothing.

## Data (Rust, `src/web/issue.rs`)

One helper `issue_toc(&IssueView, current: TocPosition) -> Toc` used by all
four handlers, passed to their templates as `toc`:

```rust
struct Toc { date: Date, display_date: String, issue_number: i64,
             items: Vec<TocItem>, position: usize, total: usize, issue_href: String }
enum TocKind { Brief, Section, Chapter, World, Behind, Colophon }
struct TocItem { kind: TocKind, label: String, href: String, number: Option<usize>,
                 minutes: Option<i64>, current: bool }
```

Chapters are the picks in issue order (numbered 1..n across sections, section
labels interleaved as non-links), followed by World Briefing and Behind the
paper when available (`has_world`/`has_behind` as computed after step 1) and a
Colophon anchor (`/issues/{date}#colophon`). `position`/`total` count
navigable chapters only (articles + world + behind), so the article page can say
"Chapter 4 of 11"; on the issue page position is 0 ("Front page").

## Markup and behaviour

- Desktop (`lg:` and up): a two-column grid, `aside` on the left `w-64 xl:w-72`,
  `sticky top-4`, `max-h-[calc(100vh-2rem)] overflow-y-auto`, thin scrollbar.
  Header: issue number + date (link to the issue), a progress line "Chapter 4
  of 11" and a 2px progress bar (width = position/total). Then the list:
  section labels as tracked uppercase sans, chapters as numbered links in
  Newsreader `text-[0.95rem]` with the read-time in muted sans, the current one
  bold with a 2px accent bar on the left and `aria-current="page"`; World
  Briefing / Behind the paper / Colophon after a hairline.
- Mobile (below `lg:`): the aside becomes a sticky bar under the site header:
  a hamburger `<button type="button" aria-controls="toc" aria-expanded="false"
  aria-label="Contents">` with the current chapter title truncated beside it and
  "4 / 11" on the right. Tapping toggles a panel (`id="toc"`, `hidden`) that
  drops down under the bar with the same list; close on link tap, on Escape,
  and on tap outside. Body scroll stays enabled. Implement in `app.js` (no
  inline handlers), progressive: without JS the panel is simply open below the
  bar. Use the same `<nav>` markup for both breakpoints (one element, styled
  responsively), not two copies.
- On article pages `app.js` also updates the progress bar with scroll progress
  within the chapter (position-1 + scrolled fraction over total), throttled with
  `requestAnimationFrame`, and honours `prefers-reduced-motion` (no transition).
- The reading column keeps its `max-w-[68ch]`; the grid must not push it
  narrower than ~60ch at `lg`. At `xl` the aside gets its full width.
- Sidebar links to the current article do nothing surprising (`href` still
  set). Keyboard: Tab order is bar → panel → content; the toggle is reachable.

## Tests

Router tests: the toc appears for `reader` on all four pages with the right
`aria-current` item and "Chapter N of M"; it is absent on public pages and in
the anonymous issue page; the legacy issue (no `issue_json`) lists World
Briefing and Behind the paper when step 1's fallback finds them and omits
World when there is no EPUB. Unit test for `issue_toc` numbering across
sections.

Finish: rebuild CSS (`npm run css` + `css:check`), `cargo fmt`, clippy
`-D warnings`, `cargo test`; screenshots of the four pages at desktop and
mobile in both themes (panel closed and open); `handoff-step4.md`; one commit:
`Web dashboard v2 step 4: table-of-contents sidebar`.
