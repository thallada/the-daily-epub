# Step 2 — Tailwind v4 design system, theme toggle, reader pages

Worktree: `/home/thallada/workspace/the-daily-epub-v2b` (branch `v2-frontend`).
Read `00-shared.md` first, especially the design language — it is the spec.
You own the design system and the **reader-facing** templates. Do not restyle
`src/web/templates/dashboard/*` (step 3 does that, on top of your system); they
may look plain until then, which is expected. Do not implement the sidebar
table of contents (step 4).

## 1. Tailwind toolchain (no Node at build or run time)

- `package.json` at the repo root with `devDependencies`
  `tailwindcss` and `@tailwindcss/cli` pinned to `4.3.3`, scripts
  `css` (`tailwindcss -i src/web/tailwind.css -o src/web/static/app.css --minify`),
  `css:watch`, and `css:check` (build to a temp file and `diff` against the
  committed `app.css`, non-zero on drift). `package-lock.json` committed;
  `node_modules/` and `dev/` added to `.gitignore`.
- The compiled `src/web/static/app.css` is **committed** and embedded with
  `include_str!` exactly as today, so `cargo build` and the server never need
  Node. Document the workflow (edit templates/`tailwind.css` → `npm run css` →
  commit both) in `README.md` and in `docs/runbooks/web-dashboard-rollout.md`.
- Fonts: `src/web/static/fonts/Newsreader.woff2`, `Newsreader-italic.woff2` and
  `OFL.txt` are already in your worktree. Serve them from
  `web::static_asset` (`include_bytes!`, `font/woff2`, same ETag/304 logic,
  long cache) — extend the route so `/static/fonts/Newsreader.woff2` or
  `/static/Newsreader.woff2` works (pick one, keep `safe` matching: a fixed
  match arm per file, no path joins). `@font-face` in `tailwind.css` with
  `font-weight: 200 800; font-stretch: normal; font-display: swap;` and the
  `unicode-range` for latin from Google's CSS
  (`U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, U+0304, U+0308, U+0329, U+2000-206F, U+20AC, U+2122, U+2191, U+2193, U+2212, U+2215, U+FEFF, U+FFFD`).
  Add a `<link rel="preload" as="font" type="font/woff2" crossorigin>` for the
  regular face in the layout.

## 2. Theme toggle (item 2)

- `src/web/static/theme.js` (new, served like `app.js`, `include_str!`): a
  handful of lines run **synchronously in `<head>`** before paint: read
  `localStorage.theme` (`"light"`/`"dark"`, anything else = system), set
  `document.documentElement.dataset.theme` accordingly (remove the attribute for
  system). Wrap storage access in try/catch. No inline script (CSP).
- The toggle button in the layout's "ears" row: `<button type="button"
  data-theme-toggle aria-label="Theme: system">` with an inline SVG icon (sun /
  moon / auto) and a short visible label. `app.js` handles clicks: cycle
  system → light → dark → system, persist (`removeItem` for system), update the
  attribute, icon and label, and re-evaluate on `matchMedia("(prefers-color-scheme: dark)")`
  changes while in system mode. The three icons can all be in the markup with
  `hidden` toggled — remember `[hidden]` must win over any `display` utility.
- Everything themed goes through the semantic tokens; `color-scheme` follows
  the theme so native controls and scrollbars match. Check both themes in
  screenshots: contrast of muted text on paper, accent on dark paper, rating
  buttons, flash, focus rings, images in dark mode.

## 3. Design system + layout

`src/web/tailwind.css` per the shared brief (tokens, dark variant, fonts,
`@layer base` for body/typography defaults, `@layer components` for `.btn`,
`.btn-primary`, `.badge` + states (`selected loved good down excluded
shortlisted admitted eligible cleared assessed triaged reason running requested
ok degraded failed dry_run`), `.kv`, `.scroll-x`, `.prose-body`, base tables
and form controls — step 3 will consume these). Rewrite `layout.html`: ears
row, masthead, primary nav with `aria-current="page"` derived from
`page.active_nav` (check what values handlers set), the admin nav row, flash,
`<main>`, footer, scripts. Add an accessible "skip to content" link.

## 4. Reader templates (restyle in place; keep every handler field)

`issue_public.html`, `issue_full.html`, `issue_list.html`, `article.html`,
`world.html`, `behind.html`, `login.html`, `account.html`, `error.html`,
`_rating_widget.html`. The public and full issue pages are the showcase — set
them like a front page: dateline, stats line, The Brief with its drop cap,
download buttons, sections as tracked labels on rules, entries per the spec,
lead story larger, the chapter links (World Briefing · Behind the paper) as a
centred rule-bounded row, the colophon as a two-column `.kv` in small sans.
Note: the backend step adds `summary`/`why` to the public entries and it will
merge after you; leave a clear place for them in `issue_public.html` (the same
summary/why treatment as the full issue) guarded with `{% if let Some(...) %}`
**only if the field exists in your tree** — otherwise just leave a comment
`{# step 1 adds summary/why here #}` so the merge is trivial.

Article page: h1 → source link, byline/meta, "Why it's here" callout, social
line, the summary as standfirst, the body in `.prose-body`, discussion with
nested comment styling (`blockquote.reply` indent, `.comment-meta` small
sans), rating control, "Read online ↗" button, prev/next cards, back link.
Login/account: a narrow centred card-free form, labelled inputs, primary button.
Archive: months as tracked labels, issues as a hairline list with date, number,
article count.

Phones (390px): masthead scales down, nav wraps as a centred row, everything
single-column, tap targets ≥ 40px, tables scroll inside `.scroll-x`.

## 5. Verify

Run the seeded dev server (shared brief), screenshot `/`, `/issues`,
`/issues/2026-09-02`, `/issues/2026-09-02/articles/1`, `/issues/2026-09-02/world`,
`/issues/2026-09-02/behind`, `/login`, `/account` as anonymous, `reader` and
`admin`, in light and dark, desktop and mobile, and **look at them**. Fix what
looks off. Make sure `npm run css:check` passes on the committed CSS, `cargo
fmt`, clippy `-D warnings` and `cargo test` are green (minus the sandbox-only
failures), then write `handoff-step2.md` and commit once on `v2-frontend`:
`Web dashboard v2 step 2: Tailwind design system, theme toggle, reader pages`.
