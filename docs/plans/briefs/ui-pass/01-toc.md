# Task 1 — table-of-contents sidebar: mobile height, scroll sync, scrollbar

Worktree: `/home/thallada/workspace/the-daily-epub-ui1` (branch `ui-toc`). Dev port 3601.
Files you own: `src/web/templates/_toc.html`, the TOC parts of
`src/web/static/app.js` (the `[data-toc-*]` blocks), the "table-of-contents"
section of `src/web/tailwind.css`, and the index entries in
`src/web/templates/issue_full.html` (only to add data attributes / ids). Backend:
`src/web/issue.rs` `Toc`/`TocItem` if a field helps. Read the shared brief first.

The signed-in issue pages (`issue_full.html`, `article.html`, `world.html`,
`behind.html`) share `_toc.html`: a sticky bar with a hamburger below `lg`, a
sticky sidebar at `lg+`. Chapters are separate pages; the issue front page lists
every chapter in "In This Issue".

## 1. The open hamburger panel must fill the screen (operator issue 1)

Today the panel is `absolute top-full max-h-[72vh]`, so on a phone it stops
partway down with page content visible beneath it — it looks broken. Make the
open panel extend from the bottom of the sticky bar to the **bottom of the
viewport** (use `100dvh`, not `100vh`, so mobile browser chrome is accounted
for), with the chapter list scrolling inside it and the bar staying fixed above
it. Handle both states: the bar already stuck at the top of the viewport, and
the bar still in flow below the masthead on first open. Prefer a pure-CSS
solution; if the bar's on-screen position must be measured, set one CSS custom
property from app.js on open/resize and consume it in `tailwind.css`. Avoid
jumps of the underlying page when the panel opens or closes. Keep Escape, the
outside-click close, the tap-on-chapter close, and the `lg` breakpoint reset.
Content behind the panel must not scroll while it is open (e.g. toggle a class
on `<html>` that sets `overflow:hidden`, cleared on close).

## 2. Scroll-synced current chapter (operator issue 2)

On the issue front page the reader scrolls through "In This Issue"; each index
entry corresponds to a TOC chapter. As the reader scrolls, the TOC's current
marker must follow: when entry 5's card reaches the top region of the viewport,
chapters 1–4 have visibly "passed" and 5 is current. Implement a scroll-spy in
app.js (IntersectionObserver or a rAF-throttled scroll listener; either is fine,
but it must be cheap and passive):

- Mark each index entry `<li>` in `issue_full.html` with the chapter it
  represents (e.g. `data-toc-entry="{{ entry.href }}"`, matching the TOC link's
  `href`); "The Brief" section maps to the Brief item, the colophon to the
  Colophon item.
- The current entry is the last one whose top edge is above a threshold
  (roughly the sticky bar height + ~1/3 of the viewport). Before the first entry
  the Brief is current.
- Move the current styling to a single hook so JS only toggles one attribute:
  give TOC links a class (say `toc-link`) and style `.toc-link[aria-current]`
  in `tailwind.css` (keep `aria-current="page"` for the server-rendered chapter
  on chapter pages; the scroll-spy may use `aria-current="location"`). Passed
  chapters may stay ink-coloured while unread ones are `ink-2`; keep it subtle.
- Update the sticky mobile bar's label and "N / total" counter and both
  `[data-toc-progress]` bars from the same source of truth (the front page
  currently leaves the progress bar at 0; it should reflect the current chapter
  as the reader passes entries). The desktop "Chapter N of M / Front page" line
  should update too. Chapter pages keep today's progress-within-chapter logic.
- When the current item changes, **auto-scroll the TOC panel** so the current
  link is comfortably in view (there is already a `revealCurrent()`; reuse it,
  make it use `scrollTo({behavior:"smooth"})` unless `prefers-reduced-motion`,
  and make sure it works for the desktop sticky sidebar as well as the open
  mobile panel). Never scroll the window itself from this code.
- Chapter pages already mark the chapter current; make sure the sidebar reveals
  it on load (desktop and mobile) — verify on a long TOC.

## 3. Scrollbar must not overlap the TOC text (operator issue 11)

In the sidebar the scrollbar is painted over the right edge of the chapter
labels. Reserve the gutter (`scrollbar-gutter: stable`, plus right padding on
the panel so the list ends before the gutter) so the scrollbar sits to the
right of the content on every platform, including overlay-scrollbar macOS.
Keep `scrollbar-width: thin` and the rule-coloured thumb.

## Polish while you are there

- Chapter links: ≥ 44 px tall tap targets on phones (they are `py-2.5` today,
  check), hover/focus-visible states consistent with the nav.
- The hamburger icon should animate to a close icon while open using the
  cross-fade recipe (both SVGs in the DOM, one absolutely positioned; opacity
  `0→1`, scale `0.25→1`, blur `4px→0`, ~150 ms, `cubic-bezier(0.2,0,0,1)`), and
  the `aria-label` should flip to "Close contents".

## Verification

Screenshot the front page and one chapter, desktop + phone, light + dark, with
the panel open on the phone; script a scroll on the front page (Playwright
`page.mouse.wheel` or `window.scrollTo`) and screenshot the sidebar at two
scroll positions to prove the marker moves and the panel auto-scrolls. Run the
web router tests. Write `handoff-toc.md`.
