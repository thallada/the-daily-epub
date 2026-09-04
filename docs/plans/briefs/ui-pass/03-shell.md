# Task 3 — site shell: nav state, ears, runs feeds table, FOUC, faster navigation

Worktree: `/home/thallada/workspace/the-daily-epub-ui3` (branch `ui-shell`). Dev port 3603.
Files you own: `src/web/templates/layout.html`, `src/web/mod.rs` (static assets,
headers), `src/web/public.rs`, `src/web/issue.rs` (page/nav plumbing only, not
the `Toc`), `src/web/templates/dashboard/run.html`, the "dashboard shell" and
`@layer base` parts of `src/web/tailwind.css`, and `src/web/static/theme.js`.
Read the shared brief first.

## 1. "Latest" is highlighted on archived issues (operator issue 5)

Every issue page (`public::show_issue`, `issue::render_full`, `issue::article`,
`world`, `behind`) passes `"latest"` as `active_nav`, so viewing `/issues/2026-09-01`
from the archive highlights *Latest* in the site nav. Highlight *Archive*
instead whenever the issue is not the newest one. Cleanest: `issue::load` (which
has the db) records `is_latest` on `IssueView` via `db.latest_issue_date()`, and
each renderer chooses `"latest"`/`"archive"` from it; the public branch in
`show_issue` needs the same. Add a router test that fetches an older issue and
asserts `aria-current="page"` is on the Archive link and not on Latest (and the
reverse for the newest issue).

## 2. "Morning edition" ears on non-issue pages (operator issue 10)

`layout.html`'s default `{% block ears %}` prints "Morning edition" — it shows on
settings and every other dashboard/account page where it is meaningless. Make the
default empty and let issue pages keep overriding it with their dateline. On
dashboard pages show a short "Dashboard" label there instead (only when
`page.is_admin()` and the active nav is a dashboard section) so the row does not
look orphaned; anything else stays empty. Check the row keeps its height and the
theme toggle stays right-aligned when the left cell is empty. `issue_public.html`'s
empty-state hero may keep its own "Morning edition" eyebrow.

## 3. Runs page "Feeds (top 20)" overflow (operator issue 6)

`dashboard/run.html`: the Feeds card wraps its table in `.scroll-x`, whose
`> table { min-width: max-content }` lets long feed titles push the *entries*
column out of the card until the reader scrolls sideways. That table must fit the
card: `table-fixed w-full`, the feed cell `truncate` (nowrap + ellipsis) with the
full name in a `title` attribute, entries column a fixed narrow width, no
horizontal scroll container (or a `.scroll-x` variant that allows shrinking —
your call, but don't break the other tables that rely on `max-content`). Check
the Timings and Provider usage cards next to it still look right, and the
dashboard overview if it has a similar box.

## 4. Flash of unstyled content (operator issue 8)

The operator sees a brief flash of unstyled content when navigating between
pages, "or maybe just sometimes". Reproduce before fixing: with the dev server,
use Playwright to capture the first frames of a navigation with the cache
disabled and a throttled network (there are older experiments `flash.mjs` /
`flash2.mjs` in the screenshot directory named in the shared brief you may crib
from) and note what actually flashes: the fallback serif before Newsreader loads
(`font-display: swap`), a theme flip, the TOC panel, restored `details` state,
or something else. Then fix the causes you confirm. Expected fixes:

- Static assets are already versioned by content hash (`?v={{ page.asset_version }}`),
  so serve `/static/*` with `Cache-Control: public, max-age=31536000, immutable`
  (update `static_assets_use_content_hash_etags`); keep the ETag.
- Fonts: preload both Newsreader files, and change `font-display` from `swap` to
  `block` (no visible fallback; with the preload and immutable cache the block
  period is only ever paid once) — or `optional` if you can show it behaves
  better here. Explain the choice in the handoff.
- Anything initialised by app.js at the end of `<body>` that changes layout
  (e.g. `details[id]` open state restored from localStorage) should either be
  decided pre-paint in `theme.js` or made not to shift layout.

## 5. Faster, SPA-like navigation without a client router (operator issue 9)

The operator wants link clicks to feel like an instant swap rather than a full
reload, but wants to decide on HTMX (boost mode) in a separate effort. **Do not**
write a fetch-and-swap router and do not add HTMX. Deliver the standards-based
progressive enhancements that get most of the way there:

- **Cross-document view transitions**: `@view-transition { navigation: auto; }`
  in `tailwind.css` with a short (~120–150 ms) cross-fade of the page and the
  header kept stable (`view-transition-name` on the site header so the masthead
  and nav do not fade; the reader's sticky TOC bar may also stay put). Respect
  reduced motion (`@media (prefers-reduced-motion: reduce)` → no animation).
- **Speculation rules** for prerender/prefetch on hover: inline
  `<script type="speculationrules">` is blocked by the CSP, so serve a JSON rule
  set as a static asset (`/static/speculation.json`, embedded like the others)
  and send the `Speculation-Rules: "/static/speculation.json"` response header
  on HTML responses (add it in `web::security_headers` or the same layer).
  Rules: `prerender` with `eagerness: "moderate"` for same-origin reader pages
  (`/`, `/issues/*`); `prefetch` with `eagerness: "moderate"` for `/dashboard/*`
  (heavy queries — don't prerender them); exclude `/logout`, `/rate`, `/static/*`
  and anything with a query string that mutates. Both are Chromium-only today and
  no-ops elsewhere; say so in the handoff.
- Make sure `Cache-Control` on HTML stays as is (`public_cache` / no-store for
  signed-in) so prerendered pages are not stale for signed-in readers, and that
  the `[data-refresh]` job-page reload cannot run in a prerendered page
  (`document.prerendering` guard).

## Polish while you are there (shell)

- Site nav and dashboard nav links: consistent focus-visible ring (they inherit
  the base `a` rule — check it is not clipped by `overflow`), and the dashboard
  nav's active link should get the same 2 px accent underline treatment as the
  site nav rather than just a colour change.
- Theme toggle and the Sign-in/username link: ≥ 40 px tap targets.
- `.tile-num`, `.pager`, and anything that shows changing counts already use
  tabular numerals — confirm; add to `.badge` when it contains a number.

## Verification

`cargo test` (full, minus the sandbox-only failures), `npm run css:check`, and
screenshots of `/issues/2026-09-01` (archive highlighting), `/dashboard/settings`
(ears), `/dashboard/runs/<id>` (feeds card at desktop and phone width), plus your
flash reproduction before/after. Curl the headers of `/` and `/static/app.css`
and paste them in `handoff-shell.md`.
