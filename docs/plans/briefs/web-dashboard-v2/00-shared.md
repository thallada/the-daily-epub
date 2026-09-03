# Shared brief — web dashboard v2 (design pass + fixes)

The web site and operator dashboard planned in `docs/plans/2026-09-03-web-dashboard.md`
is implemented and deployed (see `docs/plans/briefs/web-dashboard/handoff-step*.md`).
This second pass, requested by the operator on 2026-09-03, does six things:

1. Replaces the hand-written `src/web/static/app.css` with a **Tailwind CSS v4**
   setup and a fresh, more refined design that keeps the simple newspaper feel.
2. Adds a **light/dark theme** that follows the system by default and can be
   overridden by the reader (persisted in `localStorage`).
3. **Public** issue pages now show each article's AI summary and its
   "Why it's here" line (a deliberate reversal of plan §7.1's "no generated text";
   article bodies, The Brief, the World Briefing and comments stay private).
4. Signed-in issue pages get a **table-of-contents sidebar** (chapters = articles)
   that collapses into a hamburger menu on mobile and shows where the reader is.
5. Signed-in issue pages always offer the **World Briefing** and **Behind the paper**
   chapters, including for issues published before `issues.issue_json` existed.
6. The **Colophon** on those legacy issues shows real numbers instead of zeros.

Items 5–6 have one root cause: `web::issue::load` falls back to a reduced
`Issue` (default `Colophon`, no `world_briefing`, default `BehindThePaper`,
`from_json == false`) whenever `issues.issue_json` is NULL — which is every
issue published before migration `0004_web.sql` ran (2026-09-03).

Each step below has its own brief (`01`…`04`). Read the original plan's §4, §7,
§8 and §16 and `docs/plans/2026-08-15-implementation-notes.md` for conventions:
runtime sqlx queries with manual row mapping, `jiff` timestamps, `thiserror`/`anyhow`
errors, askama 0.16 templates, no network in tests, rustfmt defaults, no `unwrap()`
outside tests.

## Ground rules

- You work in the git worktree named in your brief. **Never touch the main
  checkout** at `/home/thallada/workspace/the-daily-epub`. Finish by committing
  **one** commit on your worktree's branch (message prefix `Web dashboard v2 step N:`).
- Do not edit migrations. Do not change routes, auth, or the security headers
  (`web::security_headers`) — in particular the CSP stays
  `default-src 'self'; img-src * data:; style-src 'self'; script-src 'self'; …`,
  so **no inline `<style>` or `<script>`, no `style=""` attributes, no CDN**.
  Fonts and scripts are served from `/static/…` (`include_str!`/`include_bytes!`).
- Every `|safe` in a template must be one of the sanitized inputs listed in plan
  §16 (plus, after step 1, the world chapter body recovered from our own EPUB).
- Keep every hook `src/web/static/app.js` relies on: `form.rating`,
  `button[data-label]`, the `.active` class and `aria-pressed` on rating
  buttons, `form[data-confirm]`, `details[id]`, `table[data-filter]`,
  `.table-filter`, `button[data-reset]`/`data-default`, `[data-refresh]`,
  `.scroll-x`. Keep all `id`, `name`, `action` and `aria-*` attributes that
  handlers or tests use. Router tests assert on text and some class names; when
  a test breaks only because a purely cosmetic class was renamed, update the test.
- Tests: unit tests inline; router tests with `tower::ServiceExt::oneshot`
  against `server::router(...)` over a temp DB, never binding a socket.
- Keep `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  green. Finish with those three commands and report their output.
- **Sandbox note:** inside the Codex sandbox `bind()` on 127.0.0.1 is forbidden,
  so exactly these pre-existing tests fail there and must be ignored:
  `curate::llm::tests::anthropic_*` (4), `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`,
  `server::tests::*` (5 that bind a listener), and `tests/m7_server.rs`. Do not
  "fix" them. The orchestrator runs the full suite outside the sandbox. If you
  cannot run the dev server for the same reason, rely on router tests and on
  rendering templates in tests; say so in the handoff.
- This host has 4 cores and 7 GB; another agent builds Rust at the same time.
  Prefer `cargo test --lib web::` style narrow runs while iterating and one full
  `cargo test` at the end.
- Don't gold-plate. Where this brief and the code disagree, follow the code and
  note the deviation in your handoff.
- When you finish, write `docs/plans/briefs/web-dashboard-v2/handoff-step<N>.md`:
  what landed, deviations and why, anything left for later, test counts, and
  (for design steps) which pages you screenshotted.

## Previewing the site locally

`examples/seed_dev_db.rs` builds a throwaway database with two issues (No. 18 on
2026-09-01 is a *legacy* issue without `issue_json`; No. 19 on 2026-09-02 has the
full snapshot), nine articles across four sections, an `admin` and a `reader`:

```text
cargo run --example seed_dev_db -- ./dev            # ./dev is git-ignored
DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) \
  cargo run -- --config ./dev/config.toml serve      # http://127.0.0.1:3599
```

Accounts: `admin` / `adminpassword123` (dashboard, ratings), `reader` /
`readerpassword123` (full issues only). Anonymous visitors get the public pages.
Port 3499 belongs to the production server on this host — never use it.

Screenshots: a Playwright script that signs in, sets the theme and shoots a
list of paths lives at
`/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/e963db53-510f-4312-ac87-460291a92781/scratchpad/shoot/shoot.mjs`
(`node shoot.mjs <outdir> [--user admin|reader|none] [--theme light|dark] [--mobile] [--full] /path …`,
`BASE=http://127.0.0.1:3599`). If that path is unreadable from your sandbox, the
bare headless Chromium at
`~/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64/chrome-headless-shell --headless --no-sandbox --screenshot=out.png --window-size=1280,1400 URL`
works for anonymous pages. **Look at your screenshots** (the Read tool renders
PNGs) before calling a design step done.

## Design language (steps 2, 3 and 4 must follow this)

The look: a quiet, well-set morning paper. Warm off-white paper, near-black ink,
one red accent, hairline rules instead of boxes, generous whitespace, real
typographic hierarchy. Reader pages are serif; the dashboard is a sans-serif
instrument panel that shares the same paper, ink, rules and accent so the two
halves feel like one publication.

**Tailwind v4 conventions.** One source file `src/web/tailwind.css`:
`@import "tailwindcss"; @source "../web/templates"; @source "./static/app.js";`
(paths relative to the file — check they resolve), design tokens in `@theme`,
the dark variant as `@custom-variant dark (&:where([data-theme="dark"], [data-theme="dark"] *));`.
Colours are **semantic CSS variables swapped per theme**, exposed to Tailwind
through `@theme inline`, so templates write `bg-paper text-ink border-rule`
once and both themes work without `dark:` on every element:

```css
:root { --paper:#f6f3ec; --paper-2:#ece7db; --ink:#1c1b18; --ink-2:#4f4b43; --muted:#7a7568;
        --rule:#d8d2c4; --rule-strong:#1c1b18; --accent:#a3231f; --accent-hover:#7c1a16;
        --loved:#2f6f46; --good:#2f6a8f; --down:#9c3f36; --warn:#8a6d1f; color-scheme:light; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { /* dark values */ } }
:root[data-theme="dark"] { --paper:#151513; --paper-2:#1e1d1a; --ink:#ebe6da; --ink-2:#bfb9aa; --muted:#8f8a7d;
        --rule:#3a3833; --rule-strong:#ebe6da; --accent:#ef8c82; --accent-hover:#f7aaa2;
        --loved:#7fcb92; --good:#7fb8d8; --down:#ea8f84; --warn:#d9b45a; color-scheme:dark; }
@theme inline { --color-paper: var(--paper); --color-paper-2: var(--paper-2); --color-ink: var(--ink); … }
```

Put the dark values in **both** the media block and the attribute block (a
small `@mixin`-free duplication is fine). Tinted badge backgrounds use
`color-mix(in oklab, var(--loved) 14%, transparent)`.

**Type.** Reader pages: *Newsreader* (variable, optical size axis, self-hosted
from `src/web/static/fonts/Newsreader.woff2` + `Newsreader-italic.woff2`,
`font-display: swap`, fallback `"Iowan Old Style", "Palatino Linotype", Georgia, serif`,
`font-optical-sizing: auto`). Body `1.0625rem/1.65`, max measure `max-w-[68ch]`.
Headlines weight 600, `leading-[1.1]`, `tracking-[-0.01em]`: article h1
`text-4xl md:text-5xl`; index headlines `text-2xl`, lead story `text-3xl`.
UI chrome (nav, datelines, bylines, meta lines, labels, buttons, forms, the
whole dashboard): the system sans stack `font-sans` (`ui-sans-serif, system-ui,
-apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`), small
labels `text-[0.72rem] uppercase tracking-[0.12em] text-muted`. Code:
`ui-monospace, SFMono-Regular, Menlo, monospace`. Tabular numbers in tables.

**Masthead and chrome.** Centred masthead "The Daily EPUB" in Newsreader 600,
`text-5xl` (`text-3xl` on phones), `tracking-[-0.02em]`, a 1px rule above and a
3px double rule below. An "ears" row above it (sans, tiny): the dateline /
issue number on the left, the theme toggle and sign-in/account on the right.
Primary nav below the masthead as a rule-bounded row of uppercase tracked
links; the active link (`aria-current="page"`) carries a 2px accent underline.
The admin nav is a second, denser row in the same style. Footer: hairline rule,
tiny muted sans. The theme toggle is one button cycling *System → Light → Dark*
(icon + short label, `aria-label`), no dropdown.

**Reader pages.** Section headings are tracked uppercase sans labels sitting on
a rule, not big serif headings. The Brief gets a drop cap on its first paragraph
(`first-letter:` utilities, Newsreader 600, ~3 lines tall) — and nothing else
gets a drop cap. Each index entry: headline (link, no underline, hover accent),
`source · N min` meta in sans, the summary in serif at body size, then the
"Why it's here" line in italic with a 2px accent bar on its left (`border-l-2
border-accent pl-3`), then the rating control for admins. Entries are separated
by hairlines; the lead story is set larger. Article pages: h1 linking to the
source, byline/meta in sans, the summary as an italic standfirst, the body with
comfortable paragraph spacing, images `rounded-sm` with a caption-style alt
fallback, code blocks on `bg-paper-2`, blockquotes with a rule on the left,
`prev/next` as two bordered cards with "Previous"/"Next" labels. Download links
are `.btn` buttons. Links inside prose: ink text, `underline decoration-rule
underline-offset-4 hover:decoration-accent`.

**Controls.** `.btn` (sans `text-sm`, 1px `border-rule-strong`, `px-3 py-1.5`,
hover fills ink/paper, `focus-visible:ring-2 ring-accent ring-offset-2
ring-offset-paper`), `.btn-primary` filled ink. The rating widget is a
segmented control: buttons joined edge to edge, the active verdict filled ink
on paper, "clear" a muted text button. Inputs: `bg-paper border-rule rounded-sm
px-2.5 py-1.5 focus:border-ink`. Flash: `border-l-4 border-accent bg-paper-2`.
Badges: pill, tinted background + coloured text per state. Transitions 150ms on
colour only; honour `prefers-reduced-motion`.

**Dashboard.** `max-w-7xl`, sans throughout, a page header (title `text-2xl
font-semibold`, one-line description, actions on the right), stat tiles (big
tabular number, small label, optional muted delta), tables `text-sm` with sticky
`thead` on `bg-paper`, hairline row rules, `hover:bg-paper-2`, right-aligned
`tabular-nums` numerics, every table inside `.scroll-x`; filters as one inline
form of labelled controls; funnels and sparklines keep their SVG classes.
Settings keep their sticky save bar. Nothing boxed unless it is a card with a
purpose (stat tiles, job cards, settings groups).

**Layout components shared by reader and dashboard** live in
`@layer components` in `tailwind.css`: `.btn`, `.btn-primary`, `.badge` and its
state colours, `.kv` (definition-list grid), `.scroll-x`, `.prose-body`
(article/world/discussion body styling), base `table`/`th`/`td`, form controls.
Prefer utilities in templates; use a component class only when the same
composite repeats in three or more places or when server-rendered HTML we do
not control (article bodies, discussions, world chapter) needs styling.
