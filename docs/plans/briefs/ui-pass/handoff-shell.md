# Task 3 handoff — site shell

## What changed

- `src/web/issue.rs`
  - `IssueView` now records `is_latest`, computed by `issue::load` from
    `Db::latest_issue_date()`.
  - The full issue, article, World Briefing, and Behind the paper renderers now
    select `latest` or `archive` navigation state from that flag.
  - Added a router test with two issue dates proving that only the newest issue
    marks Latest `aria-current="page"` and that an older issue marks Archive.
- `src/web/public.rs`
  - Anonymous issue rendering uses the same `IssueView::is_latest` navigation
    state as signed-in issue rendering.
- `src/web/templates/layout.html`
  - The default ears block is empty except on an admin dashboard route, where it
    says `Dashboard`. Issue/account/login/archive/error templates retain their
    explicit ears; the public empty issue retains its own `Morning edition`.
  - Added the italic Newsreader preload alongside the upright preload.
  - Named the site header for cross-document view transitions.
  - Made the ears-row theme and account/sign-in controls at least 40 px tall;
    the account/sign-in link also has a 40 px minimum width.
  - Dashboard navigation links now have a 40 × 40 px minimum target and use the same 2 px accent
    active underline, ink hover, and inherited focus-visible ring as site nav.
- `src/web/templates/dashboard/run.html`
  - The Feeds table no longer uses `.scroll-x`. It is fixed-layout and full
    width, truncates feed names with the full value in `title`, and reserves a
    narrow fixed entries column. Timings and Provider usage retain their
    horizontal-scroll behavior.
- `src/web/tailwind.css`
  - Both Newsreader faces use `font-display: block`.
  - Added automatic cross-document view transitions with a 140 ms root
    cross-fade. The named site header does not fade, and reduced-motion disables
    transition animation.
  - `.badge`, `.pager`, and `.pagination` use tabular numerals. (`.tile-num`
    already did; the brief said `.pager` did too, but the code did not, so it was
    corrected.)
- `src/web/static/app.css`
  - Rebuilt from `tailwind.css` with Tailwind 4.3.3.
- `src/web/static/speculation.json`
  - Added moderate same-origin prerender document rules for `/` and
    `/issues/*`, and moderate prefetch rules for `/dashboard` and
    `/dashboard/*`.
  - Logout, rating, static, and all query-string links are excluded. Excluding
    every query is deliberately conservative and therefore also excludes any
    query that could mutate state.
- `src/web/mod.rs`
  - Serves the speculation rules with
    `application/speculationrules+json` and retains content-hash ETags.
  - All `/static/*` responses, including 304 responses, now send
    `Cache-Control: public, max-age=31536000, immutable`.
  - HTML responses send `Speculation-Rules: "/static/speculation.json"` while
    retaining their existing public/private/no-store cache behavior.
  - Added assertions for immutable cache headers, rules MIME type, both font
    preloads, the HTML speculation header, dashboard ears, and dashboard active
    underline.
- `src/web/static/app.js`
  - A `[data-refresh]` job page now waits for `prerenderingchange` before
    scheduling reload, so a prerendered page cannot reload in the background.
- `src/web/static/theme.js`
  - Continues to apply the saved theme and `has-js` in the head before first
    paint.
  - A short-lived mutation observer now restores persisted `details[id]` state
    as the parser creates disclosures, then disconnects at `DOMContentLoaded`.
    This prevents a cold/delayed `app.js` request from exposing the default
    disclosure state before restoration; `app.js` still owns toggle persistence.

## Design and implementation decisions

- Chose `font-display: block` rather than `optional`. This prevents a visible
  fallback-to-Newsreader swap and preserves the publication typeface. Preloading
  both faces plus immutable content-hashed assets limits the block cost to an
  uncached first visit.
- Kept the 140 ms transition restrained and limited to document navigation.
  The masthead/nav swap immediately instead of fading, so navigation context
  stays visually anchored.
- Added only platform progressive enhancements. There is no fetch/swap router,
  HTMX, or route/auth change.
- Speculation Rules and cross-document view transitions are progressive:
  Chromium uses them; unsupported browsers ignore them and retain normal
  navigation.

## Flash reproduction and visual verification

The requested dev server could not start inside this sandbox:

```text
$ DAILY_EPUB_SERVER__BIND=127.0.0.1:3603 cargo run -- --config ./dev/config.toml serve
Error: could not bind 127.0.0.1:3603: Operation not permitted (os error 1)
```

Therefore the throttled Playwright before/after capture and the requested light,
dark, desktop, and 390 px screenshots could not be produced. The code audit
found that theme and mobile TOC state were already applied by `theme.js` before
paint. Persisted `details` state has now also moved into that pre-paint path.
The other visible-swap candidate was Newsreader's `font-display: swap`; the
preload/cache/block changes address that path.

The requested curl calls likewise could not connect:

```text
$ curl -sS -D - -o /dev/null http://127.0.0.1:3603/
curl: (7) Failed to connect to 127.0.0.1 port 3603 after 0 ms: Couldn't connect to server

$ curl -sS -D - -o /dev/null http://127.0.0.1:3603/static/app.css
curl: (7) Failed to connect to 127.0.0.1 port 3603 after 0 ms: Couldn't connect to server
```

Router tests verify the material headers that those calls should show:

```text
GET /
content-type: text/html; charset=utf-8
cache-control: public, max-age=300
speculation-rules: "/static/speculation.json"
vary: Cookie

GET /static/app.css
content-type: text/css; charset=utf-8
cache-control: public, max-age=31536000, immutable
etag: "0b00513092d50175d3615cfd6cdfc5fed6e416f2aea118c3b0f0110a66089036"
```

Signed-in issue responses remain `private, no-store`; dashboard responses
remain `no-store`.

## Verification

- `cargo fmt -- --check` — passed.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `npm run css:check` — passed with no diff using the repository's pinned
  Tailwind 4.3.3 binary. (`npm ci` could not finish under the restricted npm
  environment, so the already-installed sibling-worktree binary was placed on
  `PATH`; that worktree was only read, never modified.)
- `python3 -m json.tool src/web/static/speculation.json` — passed.
- `node --check src/web/static/app.js` and `node --check
  src/web/static/theme.js` — passed.
- `cargo test --lib web::` — 84 passed, 0 failed.
- Dedicated newest/archive navigation test — passed.
- Dedicated dashboard settings shell/ears/active-nav test — passed.
- `cargo test` — 432 library tests passed; 13 tests failed solely because their
  mock HTTP servers could not bind loopback (`Operation not permitted`). Ten are
  the sandbox failures named in the shared brief; three newer OpenAI mock-server
  tests fail at the same listener helper for the same reason.
- `cargo test --lib` with those exact 13 bind-dependent tests skipped — 432
  passed, 0 failed, 13 filtered out.
- Non-server integration tests (`config_check`, `e2e_pipeline`, `m2_pipeline`,
  `m3_curation`, `m4_epub`) — 25 passed, 0 failed.
- `cargo test --doc` — passed (0 doctests).
- `git diff --check` — passed.

## Left open

- Run the four requested screenshot sets and throttled first-frame comparison
  outside the sandbox, where port 3603 can bind. This is also the remaining
  browser-level validation for view-transition behavior and Chrome's
  Speculation Rules panel.
- `tests/m7_server.rs` was not run because it is explicitly excluded in this
  loopback-restricted sandbox.
