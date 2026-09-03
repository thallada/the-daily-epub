# Step 2 handoff — Tailwind design system, theme toggle, reader pages

Branch `v2-frontend`, worktree `/home/thallada/workspace/the-daily-epub-v2b`, one commit.
Step 3 (dashboard restyle) and step 4 (TOC sidebar) build on what is described below.

## Landed

### Toolchain

- `package.json` (root, `private`) pins `tailwindcss` and `@tailwindcss/cli` to `4.3.3` with
  scripts `css`, `css:watch` and `css:check`. `package-lock.json` is committed.
  `.gitignore` already carried `node_modules/` and `dev/`.
- `src/web/tailwind.css` is the single source; `src/web/static/app.css` is the **committed,
  minified build** and is still embedded with `include_str!`, so `cargo build`, `cargo test`
  and the deployed binary never need Node.
- `npm run css:check` rebuilds to a temp file and `diff -u`s it against the committed
  `app.css`, exiting non-zero on drift (verified in both directions).
- Workflow documented in `README.md` and `docs/runbooks/web-dashboard-rollout.md`
  (`npm ci && npm run css:check` added to the rollout build step).

### Assets

- `web::static_asset` gained fixed match arms (no path joins) for `theme.js`,
  `Newsreader.woff2` and `Newsreader-italic.woff2`. The asset tuple is now
  `(&str, &'static [u8])` so text assets use `include_str!(...).as_bytes()` and the fonts use
  `include_bytes!`; the SHA-256 ETag / 304 / long-cache path is unchanged and its test still
  passes. URLs are flat: `/static/Newsreader.woff2`, not `/static/fonts/…`.
- `@font-face` for both faces (`font-weight: 200 800`, `font-stretch: normal`,
  `font-display: swap`, Google's latin `unicode-range`), plus a
  `<link rel="preload" as="font" type="font/woff2" crossorigin>` for the regular face.
  `document.fonts.check('600 48px Newsreader')` is true on a loaded page and the server log
  shows no 404s.

### Theme

- `src/web/static/theme.js` runs synchronously in `<head>` (external file, CSP-safe): reads
  `localStorage.theme`, sets or removes `document.documentElement.dataset.theme` before paint.
- `app.js` drives `[data-theme-toggle]`: cycles **system → light → dark → system**, persists
  (`removeItem` for system), updates the attribute, the SVG icon and the visible label, keeps
  `aria-label="Theme: <state>"`, and re-renders on `matchMedia` changes while in system mode.
- Colours are semantic CSS variables on `:root`, duplicated into
  `@media (prefers-color-scheme: dark) :root:not([data-theme="light"])` and
  `:root[data-theme="dark"]`, exposed to Tailwind via `@theme inline`, with
  `@custom-variant dark (&:where([data-theme="dark"], [data-theme="dark"] *))`.
  `color-scheme` follows the theme.

### Reader templates

`layout.html` (ears row + centred masthead with the 1px rule above and the 3px double rule
below + primary nav with `aria-current="page"` + admin nav row + flash + `<main id="content">`
+ footer + skip link), `issue_public.html`, `issue_full.html`, `issue_list.html`,
`article.html`, `world.html`, `behind.html`, `login.html`, `account.html`, `error.html` and
`_rating_widget.html` are all restyled with utilities. Every hook listed in the shared brief
(`form.rating`, `button[data-label]`, `.active`, `aria-pressed`, `form[data-confirm]`,
`details[id]`, `table[data-filter]`, `.table-filter`, `[data-refresh]`, `.scroll-x`, ids,
names, actions) is intact.

## What step 3 can use

Tokens (`@theme inline`, so both a CSS variable and a Tailwind colour utility exist):

| token | utilities |
| --- | --- |
| `--paper`, `--paper-2` | `bg-paper`, `bg-paper-2` |
| `--ink`, `--ink-2`, `--muted` | `text-ink`, `text-ink-2`, `text-muted`, `bg-ink` |
| `--rule`, `--rule-strong` | `border-rule`, `border-rule-strong` |
| `--accent`, `--accent-hover` | `text-accent`, `border-accent`, `bg-accent`, `hover:text-accent-hover` |
| `--loved`, `--good`, `--down`, `--warn` | `text-loved`, `text-good`, `text-down`, `text-warn` |
| `--font-serif` / `--font-sans` / `--font-mono` | `font-serif`, `font-sans`, `font-mono` |

Component classes in `@layer components`: `.btn`, `.btn-primary`, `.badge` + state modifiers
(`selected loved good down excluded shortlisted admitted eligible cleared assessed triaged
reason running requested ok degraded failed dry_run`), `.kv`, `.scroll-x`, `.prose-body`,
`.rating`/`.rating-prompt`/`.rating-note`, `.flash`, `.error`, `.notice`, `.dashboard`,
`.cards`/`.card`, `.muted`/`.meta`/`.num`, `.filters`, `.table-filter`, `.pager`/`.run-nav`/
`.tabs`, `pre.block`/`pre.preview`/`pre.journal`, `.funnel`, `.spark` (+ `.s0`–`.s5`, `.line`).
`@layer base` styles `table`/`thead`/`th`/`td` (sticky head on `bg-paper`, hairline rules,
`hover:bg-paper-2`, tabular numerals) and all form controls.

Repeated inline recipes step 3 will probably want: small label
`font-sans text-[0.72rem] uppercase tracking-[0.12em] text-muted`; headline
`font-semibold leading-[1.1] tracking-[-0.01em]`; reader measure
`mx-auto max-w-[68ch] px-4 sm:px-6`; dashboard shell `.dashboard` (`max-w-7xl`, sans).

## Deviations and fixes beyond the brief

- **`.block` collided with Tailwind's `block` display utility.** The legacy component was
  renamed to `pre.block`, which keeps both `<pre class="block">` dashboard usages working and
  frees `class="block"` for real layout. Before this, the article prev/next cards rendered as
  grey monospace boxes.
- **`icon.hidden = …` does not work on SVG elements** (`hidden` is an `HTMLElement` IDL
  attribute); the theme icon never changed. `app.js` now uses
  `icon.toggleAttribute("hidden", …)`. `[hidden] { display:none !important }` stays in
  `@layer base` so it beats display utilities.
- `.prose-body` lists had no markers after Preflight; added `list-disc` / `list-decimal` and a
  muted `::marker`.
- The flash bar was flush to the viewport edge while the header used a gutter; `.flash` lost
  its own `mx-auto max-w-7xl` and the layout wraps it in `mx-auto max-w-7xl px-4 sm:px-6`.
- The article prev/next grid rendered an empty first cell when there is no previous article;
  the placeholder `<span>` is gone and the next card takes `sm:col-start-2`.
- `examples/seed_dev_db.rs` now writes `profile_path` into the throwaway `./dev` directory and
  seeds it from `data/profile.md`. Without this, saving on `/dashboard/profile` while running
  the dev server **rewrites the repository's `data/profile.md`** and breaks
  `curate::profile::tests::shipped_profile_and_opml_parse`. It also fixes a pre-existing
  `clippy::manual_is_multiple_of` error in that example (`cargo clippy --all-targets
  -D warnings` did not pass on this branch before).
- `Page::new` for the Jobs and Users dashboard pages now passes `"jobs"` / `"users"` instead of
  `"dashboard"`, so the admin nav highlights the right entry.
- `FullEntry` gained `is_lead` so the full issue's lead story is set larger (`text-3xl` vs
  `text-2xl`), matching what `issue_public.html` already did.
- Fonts are served flat from `/static/<name>.woff2` (the brief allowed either shape).
- `issue_public.html` carries `{# step 1 adds summary/why here #}` at the point where the
  backend step's summary/`why` markup should go; use the same treatment as `issue_full.html`
  (summary as a body-size serif paragraph, then `border-l-2 border-accent pl-3 italic`).
- The dashboard templates are deliberately unstyled beyond what the base/component layers give
  them. They render without errors (all nine routes smoke-tested) but look plain — step 3.
- The archive month label is still the raw `YYYY-MM` produced by `web::public`; changing it is
  a backend concern and was left alone.

## Verification

- `npm run css` / `npm run css:check` — clean; drift is detected (exit 1) when `app.css` is
  edited by hand.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test` — **475 passed, 0 failed** (439 lib + 9 + 2 + 3 + 4 + 7 + 9 + 2 integration,
  one binary with 0 tests). Run outside the Codex sandbox, so the socket-binding tests ran too.
  No test needed a cosmetic-class update.
- Seeded dev server on `127.0.0.1:3599`. All of `/`, `/issues`, `/issues/2026-09-01` (legacy),
  `/issues/2026-09-02`, `/issues/2026-09-02/articles/1`, `/…/world`, `/…/behind`, `/account`,
  `/feed.xml` and the nine `/dashboard/*` routes return 200 with **no console errors and no
  asset 404s**; `/issues/1999-01-01` returns the styled 404 page.
- Theme toggle asserted with Playwright: system → light → dark → system, `localStorage.theme`
  written/removed, `data-theme` set/removed, icon and label follow, state survives a reload.
- Focus order checked: the skip link is the first tab stop and becomes visible; the toggle
  shows the accent focus ring.

### Screenshots

`/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/e963db53-510f-4312-ac87-460291a92781/scratchpad/step2-shots/`
— 83 PNGs. `/`, `/issues`, `/issues/2026-09-02`, `/issues/2026-09-02/articles/1`, `/…/world`,
`/…/behind`, `/account` and `/login` × {anonymous, reader, admin} × {light, dark} ×
{1280px desktop, 390px mobile}, full page. Named
`<path>-<user>-<theme>[-mobile].png`. Plus `toggle-1..4.png` (the toggle cycle),
`flash-light/dark.png`, `rating-active-light.png`, `focus-skiplink.png`, `focus-toggle.png`
and `issues_1999-01-01-none-{light,dark}.png` (404 page).

## Left for later

- Step 1 adds `summary`/`why` to the public entries (marker comment in place).
- Step 3 restyles `src/web/templates/dashboard/*` on top of this system.
- Step 4 adds the collapsing table-of-contents sidebar to the signed-in issue pages.
