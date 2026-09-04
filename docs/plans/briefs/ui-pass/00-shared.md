# Shared brief — UI polish pass (2026-09-04)

The operator asked for a design/bug-fix pass over the web dashboard v2 (Tailwind
redesign, merged to `main` at `e719e03`+). The work is split into three tasks
(`01-toc.md`, `02-rating.md`, `03-shell.md`) that run in parallel in separate git
worktrees. Read `docs/plans/briefs/web-dashboard-v2/00-shared.md` for the design
language (quiet morning paper: warm paper, near-black ink, one red accent,
hairline rules, real typographic hierarchy; reader pages serif, dashboard sans)
and the Tailwind v4 conventions. This brief overrides it where they disagree.

## Ground rules

- Work only in the worktree named in your brief. **Never touch the main checkout**
  at `/home/thallada/workspace/the-daily-epub`. Do not `git commit` (the sandbox
  cannot write `.git/worktrees`); leave the work in the tree and the orchestrator
  commits. If you can commit, one commit on the worktree branch is fine.
- Styling lives in **`src/web/tailwind.css`** (Tailwind v4, `@apply` in
  `@layer components`, semantic colour tokens `paper/paper-2/ink/ink-2/muted/rule/
  rule-strong/accent/loved/good/down/warn`) and in Tailwind utility classes in the
  askama templates under `src/web/templates/`. **Rebuild the committed CSS** after
  every change with `npm run css` (writes `src/web/static/app.css`; the binary
  embeds it with `include_str!`, so rebuild Rust after the CSS). `npm run css:check`
  must print no diff at the end.
- CSP is `default-src 'self'; style-src 'self'; script-src 'self'`: **no inline
  `<style>`/`<script>`, no `style=""` attributes, no CDNs.** Setting CSS custom
  properties or `element.style.*` from `app.js` (CSSOM) is allowed by CSP but
  prefer class/attribute toggles styled from `tailwind.css`.
- JS is plain `src/web/static/app.js` (runs once at the end of `<body>`) and the
  tiny pre-paint `src/web/static/theme.js`. Keep every hook app.js relies on:
  `form.rating`, `button[data-label]`, `.active` + `aria-pressed` on rating
  buttons, `button.clear`, `form[data-confirm]`, `details[id]`, `table[data-filter]`,
  `.table-filter`, `button[data-reset]`/`data-default`, `[data-refresh]`,
  `[data-toc-panel]`, `[data-toc-toggle]`, `[data-toc-progress]`, `[data-toc-scroll]`,
  `[data-theme-toggle]`. Keep `id`/`name`/`action`/`aria-*` attributes used by
  handlers or tests. Router tests assert on text and some class names; if a test
  breaks only because a cosmetic class changed, update the test.
- Don't change routes, auth, migrations or `web::security_headers`.
- Prefer CSS transitions (interruptible) over keyframes; name exact transition
  properties (never `transition: all`); respect the existing
  `prefers-reduced-motion` rule. Touch targets ≥ 44 px on phones (extend with a
  pseudo-element if the visible control is smaller); ≥ 40 px in the dense
  dashboard. Tabular numerals for changing numbers. Hairline rules, not boxes,
  for structure. No decorative motion on frequent interactions.
- Keep `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests
  green. Iterate with narrow runs (`cargo test --lib web::`), one `cargo test` at
  the end. **Sandbox note:** inside the Codex sandbox `bind()` on 127.0.0.1 is
  forbidden, so these pre-existing tests fail there and must be ignored:
  `curate::llm::tests::anthropic_*` (4), `extract::tests::relative_urls_resolve_against_the_url_we_landed_on`,
  `server::tests::*` (5) and `tests/m7_server.rs`. Do not "fix" them.
- This host has 4 cores / 7 GB and other agents build Rust at the same time; be
  patient with `cargo` and don't run several builds concurrently yourself.
- Don't gold-plate and don't widen scope beyond your brief's numbered items plus
  the polish items it lists. Where the brief and the code disagree, follow the
  code and note the deviation.
- Finish by writing `docs/plans/briefs/ui-pass/handoff-<task>.md`: what changed
  (file by file, briefly), design decisions, deviations, what you verified and
  how (commands, screenshots), anything left open.

## Previewing

A seeded dev database exists in the main checkout at
`/home/thallada/workspace/the-daily-epub/dev` (two issues: No. 18 on 2026-09-01,
legacy; No. 19 on 2026-09-02, full snapshot; accounts `admin`/`adminpassword123`
and `reader`/`readerpassword123`). Copy it if you need your own:
`cp -r /home/thallada/workspace/the-daily-epub/dev ./dev` (git-ignored). Run your
worktree's server on **your own port** (given in your brief; 3499 is production,
3599 is the orchestrator's):

```text
DAILY_EPUB_SERVER__HMAC_SECRET=$(head -c 48 /dev/urandom | base64) \
DAILY_EPUB_SERVER__BIND=127.0.0.1:<port> \
  cargo run -- --config ./dev/config.toml serve
```

(the listen address is `server.bind` in `./dev/config.toml`, overridable by the
env var above; your worktree's `dev/config.toml` is already set to your port. If
the sandbox forbids binding, rely on router tests + `cargo test` and say so in
the handoff).

Screenshots: `node shoot.mjs <outdir> [--user admin|reader|none] [--theme light|dark] [--mobile] [--full] /path …`
with `BASE=http://127.0.0.1:<port>`, from
`/tmp/claude-1000/-home-thallada-workspace-the-daily-epub/553c7d77-d8f4-44f8-a379-73cedea23814/scratchpad/shoot/`
(Playwright with a headless Chromium already installed under `~/.cache/ms-playwright`).
If that directory is unreadable, the bare browser at
`~/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64/chrome-headless-shell --headless --no-sandbox --screenshot=out.png --window-size=1280,1400 URL`
works for anonymous pages. **Look at your screenshots** (read the PNGs) before
calling a design change done, in light and dark, desktop and 390 px phone.

Useful dev URLs: `/` (latest issue, No. 19), `/issues/2026-09-01` (archived
issue), `/issues/2026-09-02/articles/<id>` (chapter; pick ids from the TOC),
`/issues/2026-09-02/world`, `/issues/2026-09-02/behind`, `/dashboard`,
`/dashboard/runs`, `/dashboard/runs/<id>`, `/dashboard/ratings`,
`/dashboard/settings`, `/dashboard/articles`.
